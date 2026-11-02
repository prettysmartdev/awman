//! One frontend-release conformance check, run against every runtime
//! (WI 0126 B2).
//!
//! The contract on [`AgentInstance::run_with_frontend`] and
//! [`AgentExecution::wait`]: a runtime MUST drop the frontend, and the I/O
//! sinks it handed out, no later than when `wait()` resolves, when the
//! `wait()` future is dropped, or when the execution is dropped. Each runtime
//! contributes a [`Launcher`] driving its own fake (a fake container CLI, a
//! fake guest exec stream, a plain child process for dsbx), and
//! [`assert_releases_frontend`] holds every one of them to the same checks.
//!
//! [`AgentInstance::run_with_frontend`]: super::execution::AgentInstance::run_with_frontend

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use super::execution::AgentExecution;
use super::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::container::options::PromptFileGuard;

type LaunchFn =
    Box<dyn FnMut(Box<dyn AgentFrontend>, bool, Vec<PromptFileGuard>) -> AgentExecution + Send>;

/// Starts one agent on a runtime's fake.
pub(crate) struct Launcher {
    pub name: &'static str,
    /// Launch an agent that exits promptly (`false`) or keeps running until
    /// `finish` is called (`true`), owning the given prompt files. dsbx
    /// delivers prompts inline and ignores them.
    pub launch: LaunchFn,
    /// End a still-running agent started with `true`.
    pub finish: Box<dyn FnMut() + Send>,
}

type OutputReceivers = (UnboundedReceiver<Vec<u8>>, UnboundedReceiver<Vec<u8>>);
type SharedReceivers = Arc<Mutex<Option<OutputReceivers>>>;

/// A frontend that records when it is dropped (slowly, so a runtime that
/// drops it only after publishing the exit is caught) and keeps the
/// receiving ends of the I/O it hands out, so a test can see the runtime
/// release the sending ends.
struct ReleaseProbe {
    released: Arc<AtomicBool>,
    receivers: SharedReceivers,
}

impl Drop for ReleaseProbe {
    fn drop(&mut self) {
        std::thread::sleep(Duration::from_millis(20));
        self.released.store(true, Ordering::SeqCst);
    }
}

impl crate::data::message::UserMessageSink for ReleaseProbe {
    fn write_message(&mut self, _: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for ReleaseProbe {
    fn report_status(&mut self, _: AgentStatus) {}
    fn report_progress(&mut self, _: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        let (stdout, stdout_rx) = unbounded_channel();
        let (stderr, stderr_rx) = unbounded_channel();
        let (stdin_tx, stdin_rx) = unbounded_channel();
        *self.receivers.lock().unwrap() = Some((stdout_rx, stderr_rx));
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

fn probe() -> (Box<dyn AgentFrontend>, Arc<AtomicBool>, SharedReceivers) {
    let released = Arc::new(AtomicBool::new(false));
    let receivers = Arc::new(Mutex::new(None));
    (
        Box::new(ReleaseProbe {
            released: released.clone(),
            receivers: receivers.clone(),
        }),
        released,
        receivers,
    )
}

/// Both output streams end: every sender the runtime held is gone.
async fn assert_io_released(name: &str, receivers: &Mutex<Option<OutputReceivers>>) {
    let Some((mut stdout, mut stderr)) = receivers.lock().unwrap().take() else {
        // This runtime never asked the frontend for I/O.
        return;
    };
    for (stream, rx) in [("stdout", &mut stdout), ("stderr", &mut stderr)] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(None) => break,
                Ok(Some(_)) => continue,
                Err(_) => panic!("{name}: the {stream} sink is still held"),
            }
        }
    }
}

/// The frontend-release contract, for one runtime.
pub(crate) async fn assert_releases_frontend(launcher: &mut Launcher) {
    let name = launcher.name;

    // `wait()` resolves: the frontend is gone by then.
    let (frontend, released, receivers) = probe();
    let mut execution = (launcher.launch)(frontend, false, Vec::new());
    let _ = tokio::time::timeout(Duration::from_secs(10), execution.wait())
        .await
        .unwrap_or_else(|_| panic!("{name}: a prompt agent never exited"));
    assert!(
        released.load(Ordering::SeqCst),
        "{name}: frontend retained after wait() resolved"
    );
    assert_io_released(name, &receivers).await;
    drop(execution);

    // The `wait()` future is dropped while the agent still runs (an abort):
    // the frontend and its I/O are released promptly, not at guest exit.
    let (frontend, released, receivers) = probe();
    let mut execution = (launcher.launch)(frontend, true, Vec::new());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), execution.wait())
            .await
            .is_err(),
        "{name}: a long-running agent exited early"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !released.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "{name}: frontend retained after the wait() future was dropped"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_io_released(name, &receivers).await;
    (launcher.finish)();
    drop(execution);
}

fn prompt_file() -> (PromptFileGuard, std::path::PathBuf) {
    let guard = PromptFileGuard::new(tempfile::NamedTempFile::new().unwrap());
    let path = guard.host_path().to_path_buf();
    (guard, path)
}

async fn assert_eventually_removed(name: &str, path: &std::path::Path, after: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{name}: prompt file retained after {after}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// An execution owns its launch's prompt files: present while it runs, gone
/// when `wait()` resolves, and released (not leaked) after a dropped wait or a
/// dropped execution once the agent has ended.
pub(crate) async fn assert_releases_prompt_files(launcher: &mut Launcher) {
    let name = launcher.name;

    let (guard, path) = prompt_file();
    let mut execution = (launcher.launch)(probe().0, false, vec![guard]);
    assert!(path.exists(), "{name}: prompt file missing after launch");
    tokio::time::timeout(Duration::from_secs(10), execution.wait())
        .await
        .unwrap_or_else(|_| panic!("{name}: a prompt agent never exited"))
        .unwrap();
    assert!(
        !path.exists(),
        "{name}: prompt file retained after wait() resolved"
    );
    drop(execution);

    let (guard, path) = prompt_file();
    let mut execution = (launcher.launch)(probe().0, true, vec![guard]);
    assert!(path.exists(), "{name}: prompt file missing after launch");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), execution.wait())
            .await
            .is_err(),
        "{name}: a long-running agent exited early"
    );
    (launcher.finish)();
    assert_eventually_removed(name, &path, "a dropped wait").await;
    drop(execution);

    let (guard, path) = prompt_file();
    let execution = (launcher.launch)(probe().0, true, vec![guard]);
    assert!(path.exists(), "{name}: prompt file missing after launch");
    drop(execution);
    (launcher.finish)();
    assert_eventually_removed(name, &path, "a dropped execution").await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_runtime_releases_its_frontend_on_normal_and_dropped_wait() {
    let mut launchers = crate::engine::container::conformance_launchers();
    launchers.push(crate::engine::sandbox::conformance_launcher());
    let names: Vec<_> = launchers.iter().map(|l| l.name).collect();
    assert_eq!(names, ["docker", "apple", "builtin", "dsbx"]);
    for mut launcher in launchers {
        assert_releases_frontend(&mut launcher).await;
        if launcher.name != "dsbx" {
            assert_releases_prompt_files(&mut launcher).await;
        }
    }
}
