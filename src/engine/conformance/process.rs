//! Host process conformance: awman's process runner
//! (`ProcessHostCommandRunner`, the implementation behind every streamed
//! host CLI call such as image builds) against one program provider.
//!
//! Contracts (`L1-PROC-*`): the exit status is relayed; stdout and stderr
//! lines both reach the line callback, each stream in order; a cancelled
//! token stops a running child promptly and never spawns once cancelled; a
//! deadline ends a child that keeps running; a missing program is the typed
//! runtime-unavailable error; a descendant that keeps the output pipes open
//! cannot stall the call past its exit (bounded drain).

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{ensure, Report, Waiter};
use crate::engine::container::host_build::{HostCommandRunner, ProcessHostCommandRunner};
use crate::engine::container::BuildControl;
use crate::engine::error::EngineError;
use crate::engine::oci::CancelToken;

/// What a world's program must do for one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessCase {
    /// Print `out-1`, `out-2` on stdout and `err-1` on stderr; exit 7.
    Streams,
    /// Keep running until killed (no output required).
    Hang,
    /// Print `parent-done` on stdout, leave a descendant that holds stdout
    /// and stderr open, and exit 0.
    LeavesDescendant,
}

/// The world a process runner suite runs in.
pub trait ProcessWorld: Waiter {
    /// Name of the program provider, e.g. `fake CLI` or `/bin/sh`.
    fn subject(&self) -> String;
    /// A command that behaves as `case` describes. It is built from the
    /// instance's snapshot (`engine::host_cli::command`), as awman builds it.
    fn command(&self, case: ProcessCase) -> Command;
    /// A command whose program does not exist.
    fn missing_program(&self) -> Command;
    /// Whether the program started for `case` is running now.
    fn is_running(&self, case: ProcessCase) -> bool;
    /// Stop anything the world started (descendants included).
    fn cleanup(&self);
}

/// Generous bound for "promptly": far above scheduling noise, far below a hang.
const PROMPT: Duration = Duration::from_secs(15);
/// The deadline the deadline check sets.
const DEADLINE: Duration = Duration::from_millis(500);

/// Run every process runner check against `world`.
pub fn host_command_runner(world: &dyn ProcessWorld) -> Report {
    let mut report = Report::new(world.subject());
    report.check(
        "exit_status_and_both_streams_are_relayed",
        check_streams(world),
    );
    report.check("a_cancelled_token_never_spawns", check_cancel_before(world));
    report.check(
        "cancellation_stops_a_running_child_promptly",
        check_cancel_running(world),
    );
    report.check(
        "a_deadline_ends_a_child_that_keeps_running",
        check_deadline(world),
    );
    report.check(
        "a_missing_program_is_runtime_unavailable",
        check_missing(world),
    );
    report.check(
        "a_descendant_holding_the_pipes_cannot_stall_the_call",
        check_descendant(world),
    );
    world.cleanup();
    report
}

fn run(
    command: Command,
    control: &BuildControl,
) -> (Result<std::process::ExitStatus, EngineError>, Vec<String>) {
    let mut lines = Vec::new();
    let result = ProcessHostCommandRunner.run(command, control, &mut |l| lines.push(l.to_string()));
    (result, lines)
}

fn check_streams(world: &dyn ProcessWorld) -> Result<(), String> {
    let control = BuildControl::unbounded(CancelToken::new());
    let (result, lines) = run(world.command(ProcessCase::Streams), &control);
    let status = result.map_err(|e| format!("run failed: {e}"))?;
    ensure(status.code() == Some(7), || {
        format!("exit {:?}, expected 7", status.code())
    })?;
    let out: Vec<&String> = lines.iter().filter(|l| l.starts_with("out-")).collect();
    ensure(out == ["out-1", "out-2"], || {
        format!("stdout lines {out:?} in {lines:?}")
    })?;
    ensure(lines.iter().any(|l| l == "err-1"), || {
        format!("stderr line missing: {lines:?}")
    })
}

fn check_cancel_before(world: &dyn ProcessWorld) -> Result<(), String> {
    let token = CancelToken::new();
    token.cancel();
    let (result, _) = run(
        world.command(ProcessCase::Hang),
        &BuildControl::unbounded(token),
    );
    ensure(result.is_err(), || {
        "a cancelled control still ran the program".to_string()
    })?;
    ensure(!world.is_running(ProcessCase::Hang), || {
        "the program was started although the token was cancelled".to_string()
    })
}

fn check_cancel_running(world: &dyn ProcessWorld) -> Result<(), String> {
    let token = CancelToken::new();
    let control = BuildControl::unbounded(token.clone());
    let command = world.command(ProcessCase::Hang);
    type Finished = Option<(Result<(), String>, Instant)>;
    let finished: Arc<Mutex<Finished>> = Arc::default();
    let slot = Arc::clone(&finished);
    let worker = std::thread::spawn(move || {
        let (result, _) = run(command, &control);
        let outcome = match result {
            Err(_) => Ok(()),
            Ok(status) => Err(format!("cancelled child reported {status}")),
        };
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some((outcome, Instant::now()));
    });
    let started = world.wait_until("hanging child running", PROMPT, &mut || {
        world.is_running(ProcessCase::Hang)
    });
    let cancelled_at = Instant::now();
    token.cancel();
    let done = world.wait_until("cancelled run returns", PROMPT, &mut || {
        finished.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    });
    if !done {
        world.cleanup();
    }
    let _ = worker.join();
    ensure(started, || "the child never started".to_string())?;
    ensure(done, || {
        format!("run still blocked {PROMPT:?} after cancel")
    })?;
    let (outcome, at) = finished
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .ok_or("no outcome")?;
    outcome?;
    ensure(at.duration_since(cancelled_at) < PROMPT, || {
        "cancel was not prompt".to_string()
    })?;
    let gone = world.wait_until("cancelled child reaped", PROMPT, &mut || {
        !world.is_running(ProcessCase::Hang)
    });
    ensure(gone, || "the cancelled child is still running".to_string())
}

fn check_deadline(world: &dyn ProcessWorld) -> Result<(), String> {
    let control = BuildControl::new(CancelToken::new(), DEADLINE);
    let (result, _) = run(world.command(ProcessCase::Hang), &control);
    match result {
        Err(EngineError::ImageBuildTimedOut) => {}
        other => return Err(format!("expected ImageBuildTimedOut, got {other:?}")),
    }
    let gone = world.wait_until("timed-out child reaped", PROMPT, &mut || {
        !world.is_running(ProcessCase::Hang)
    });
    ensure(gone, || "the timed-out child is still running".to_string())
}

fn check_missing(world: &dyn ProcessWorld) -> Result<(), String> {
    let control = BuildControl::unbounded(CancelToken::new());
    match run(world.missing_program(), &control).0 {
        Err(EngineError::ContainerRuntimeUnavailable { .. }) => Ok(()),
        other => Err(format!(
            "expected ContainerRuntimeUnavailable, got {other:?}"
        )),
    }
}

fn check_descendant(world: &dyn ProcessWorld) -> Result<(), String> {
    let control = BuildControl::unbounded(CancelToken::new());
    let started = Instant::now();
    let (result, lines) = run(world.command(ProcessCase::LeavesDescendant), &control);
    let took = started.elapsed();
    world.cleanup();
    let status = result.map_err(|e| format!("run failed: {e}"))?;
    ensure(status.success(), || format!("exit {status}"))?;
    ensure(lines.iter().any(|l| l == "parent-done"), || {
        format!("output before exit was lost: {lines:?}")
    })?;
    ensure(took < PROMPT, || {
        format!("call stalled {took:?} on inherited pipes")
    })
}
