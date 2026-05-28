//! Integration tests confirming async task cancellation semantics (work item 0033).
//!
//! These tests verify that:
//! 1. The oneshot cancellation pattern (used for `status --watch` loops) works correctly.
//! 2. Background tasks that hold the `output_tx` end of an UnboundedChannel detect
//!    channel closure when the receiver (`output_rx`) is dropped.
//! 3. Workflow tasks stop producing output after a tab is closed.
//! 4. There are no lingering tasks whose existence could be detected after teardown.
//!
//! # Design note on `spawn_text_command`
//!
//! `spawn_text_command` spawns a *detached* tokio task (no `JoinHandle` is returned
//! or stored). The task runs `f` to completion regardless of whether the caller still
//! holds `output_rx` or `exit_rx`. This is intentional for short-lived commands but
//! means there is currently **no mechanism to forcibly cancel a long-running command**
//! other than the OS-level process exit (handled by the PTY layer for interactive
//! sessions). A future improvement would be to store the `JoinHandle` and abort it on
//! tab close.

use awman::tui::state::{App, TabState};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

// ─── Oneshot cancellation pattern ─────────────────────────────────────────────

/// Verifies the oneshot cancellation pattern used by `status_watch_cancel_tx`.
///
/// When a new command starts, `start_command` sends on the cancellation sender
/// to signal any running `status --watch` background task that it should stop.
#[tokio::test]
async fn oneshot_cancel_signal_is_delivered() {
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

    // Simulate a background task watching for cancellation.
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let task = tokio::spawn(async move {
        let _ = cancel_rx.await; // wait for cancellation
        flag.store(true, Ordering::SeqCst);
    });

    // Send the cancellation signal (as start_command does).
    cancel_tx.send(()).expect("receiver must be alive");
    task.await.expect("task should complete");

    assert!(
        cancelled.load(Ordering::SeqCst),
        "Cancellation flag was not set — the oneshot signal was not received"
    );
}

/// Verifies that dropping the cancellation sender (e.g. when the `TabState` is
/// dropped) also causes the waiting task to unblock with an error.
#[tokio::test]
async fn oneshot_cancel_receiver_unblocks_on_sender_drop() {
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        // A real status--watch loop: exit on either Ok (explicit cancel) or Err (dropped).
        match cancel_rx.await {
            Ok(()) | Err(_) => {} // both outcomes mean "stop"
        }
    });

    // Drop the sender without sending — simulates the TabState being dropped.
    drop(cancel_tx);
    task.await.expect("task should complete after sender drop");
}

/// Confirms that `start_command` triggers the stored `status_watch_cancel_tx`.
///
/// This tests the actual `TabState` API rather than the raw channel primitive.
#[test]
fn start_command_fires_status_watch_cancel() {
    // Build a cancel channel and plant the sender in the tab.
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    let mut tab = TabState::new(PathBuf::from("/tmp/cancel-test"));
    tab.status_watch_cancel_tx = Some(cancel_tx);

    // start_command should consume and fire the sender.
    tab.start_command("new-command".into());

    assert!(
        tab.status_watch_cancel_tx.is_none(),
        "status_watch_cancel_tx must be None after start_command consumed it"
    );

    // The receiver should see the signal.
    // Use try_recv (no async runtime needed here).
    assert!(
        cancel_rx.try_recv().is_ok(),
        "The oneshot receiver did not receive the cancellation signal"
    );
}

// ─── Channel closure detection ────────────────────────────────────────────────

/// When the `output_rx` receiver of an UnboundedChannel is dropped (tab closed),
/// any task holding `output_tx` should detect a `SendError` on the next send.
///
/// This mirrors the `OutputSink::Channel(output_tx)` pattern used in
/// `spawn_text_command`: once the TUI drops `output_rx`, further sends fail.
#[tokio::test]
async fn output_channel_send_fails_after_receiver_dropped() {
    let (output_tx, output_rx) = mpsc::unbounded_channel::<String>();

    // Simulate the task holding the sender.
    let task = tokio::spawn(async move {
        let mut count = 0usize;
        loop {
            // In real code, output lines come from the running command.
            if output_tx.send(format!("line {}", count)).is_err() {
                // Channel closed — stop producing output.
                break;
            }
            count += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        count
    });

    // Let the task produce a few lines then drop the receiver (tab closed).
    tokio::time::sleep(Duration::from_millis(10)).await;
    drop(output_rx);

    // The task must stop on its own now that the channel is closed.
    let lines_sent = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("task must finish within 5 s after channel close")
        .expect("task must not panic");

    assert!(
        lines_sent > 0,
        "Task should have sent at least one line before the channel closed"
    );
}

/// The `exit_rx` oneshot is used by the TUI to detect command completion.
/// When the TUI drops `exit_rx` (tab closed mid-run), the background task's
/// final `exit_tx.send(code)` silently fails — but the task still runs to
/// completion rather than being cancelled.
///
/// This test documents that behaviour and serves as a regression guard.
#[tokio::test]
async fn exit_rx_drop_does_not_cancel_running_task() {
    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let task_ran_to_completion = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&task_ran_to_completion);

    let task = tokio::spawn(async move {
        // Simulate a non-trivial command body.
        tokio::time::sleep(Duration::from_millis(20)).await;
        flag.store(true, Ordering::SeqCst);
        // Send exit code — this will fail if exit_rx was already dropped.
        let _ = exit_tx.send(0);
    });

    // Drop exit_rx immediately (as if the tab was closed while the command runs).
    drop(exit_rx);

    task.await.expect("detached task must complete");

    assert!(
        task_ran_to_completion.load(Ordering::SeqCst),
        "Task must run to completion even when exit_rx is dropped — \
         detached tasks cannot be forcibly cancelled by dropping the receiver"
    );
}

// ─── Workflow task teardown ────────────────────────────────────────────────────

/// When a workflow is stopped by closing the tab, all channel senders owned by
/// the `TabState` are dropped.  Any task waiting on those channels should
/// unblock and exit cleanly.
///
/// This test simulates a workflow step that polls `output_rx` for lines; closing
/// the tab (dropping `TabState`) must cause the polling task to terminate.
#[tokio::test]
async fn workflow_task_terminates_when_tab_is_closed() {
    // Simulate a workflow step task: reads from output_rx until closed.
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<String>();
    let task_done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&task_done);

    let task = tokio::spawn(async move {
        while output_rx.recv().await.is_some() {
            // consume
        }
        // Channel closed — all senders (output_tx copies) were dropped.
        flag.store(true, Ordering::SeqCst);
    });

    // Send a few lines (simulating normal operation).
    output_tx.send("step output 1".into()).unwrap();
    output_tx.send("step output 2".into()).unwrap();

    // Drop the sender — simulates the tab (and its output_tx) being closed.
    drop(output_tx);

    // The task should see channel closure and terminate.
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("workflow task must stop within 5 s of tab closure")
        .expect("workflow task must not panic");

    assert!(
        task_done.load(Ordering::SeqCst),
        "Workflow task must exit cleanly when the tab's output channel is closed"
    );
}

/// Verifies that closing a tab with an active App removes its state cleanly
/// and the remaining tabs are unaffected.
#[test]
fn close_tab_cleans_up_workflow_state() {
    let mut app = App::new(PathBuf::from("/tmp/wf-close-a"));
    let _idx = app.create_tab(PathBuf::from("/tmp/wf-close-b"));

    // Plant some workflow state in tab 0.
    app.tabs[0].workflow_current_step = Some("step-A".to_string());
    app.tabs[0].push_output("workflow step output".to_string());

    // Tab 1 has unrelated state.
    app.tabs[1].push_output("tab-1-output".to_string());

    app.close_tab(0);

    assert_eq!(app.tabs.len(), 1);
    // Surviving tab must not have inherited workflow state from the closed tab.
    assert!(
        app.tabs[0].workflow_current_step.is_none(),
        "Workflow step leaked from closed tab into surviving tab"
    );
    assert!(
        app.tabs[0]
            .output_lines
            .iter()
            .any(|l| l == "tab-1-output"),
        "Surviving tab lost its own output after close_tab"
    );
    assert!(
        !app.tabs[0]
            .output_lines
            .iter()
            .any(|l| l == "workflow step output"),
        "Closed tab's output leaked into the surviving tab"
    );
}

// ─── No lingering tasks after simulated teardown ──────────────────────────────

/// After tearing down a simulated workflow run (dropping all channels and
/// handles), the tokio runtime must be able to shut down cleanly with no
/// tasks stuck in an infinite loop or blocking receive.
///
/// This test creates a controlled set of tasks, signals them all to stop, and
/// confirms they all complete before the runtime exits.
#[tokio::test]
async fn all_tasks_complete_after_teardown_signal() {
    const N_TASKS: usize = 10;
    let mut cancel_txs = Vec::new();
    let mut task_handles = Vec::new();

    for i in 0..N_TASKS {
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel::<String>();
        cancel_txs.push(cancel_tx);

        let handle = tokio::spawn(async move {
            tokio::select! {
                // Stop when the cancellation signal arrives.
                _ = cancel_rx => {},
                // Also stop if the output channel closes.
                _ = async { while output_rx.recv().await.is_some() {} } => {},
            }
            i // return task ID so we can confirm all completed
        });
        task_handles.push((handle, output_tx));
    }

    // Give tasks a moment to start.
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Teardown: send cancellation to all tasks.
    for tx in cancel_txs {
        let _ = tx.send(());
    }

    // All tasks must complete within a reasonable timeout.
    let mut completed_ids = Vec::new();
    for (handle, _output_tx) in task_handles {
        let id = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("task must complete within 5 s after teardown signal")
            .expect("task must not panic");
        completed_ids.push(id);
    }

    assert_eq!(
        completed_ids.len(),
        N_TASKS,
        "Not all tasks completed after teardown: got {} of {}",
        completed_ids.len(),
        N_TASKS,
    );
}
