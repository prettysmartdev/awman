//! Interactive setup/teardown containers.
//!
//! A setup/teardown shell step normally runs headless: a background container
//! is started with an idle entrypoint, the step's command is `exec`'d into it
//! with no PTY, and each output line is streamed to the frontend's message
//! bus. When the frontend has a terminal to offer
//! ([`ExecWorkflowCommandFrontend::supports_interactive_phase_steps`] — the
//! CLI on a TTY and the TUI, never the API server or the squad daemon), the
//! step runs here instead: the command *is* the container's main process, in a
//! foreground container attached to a PTY exactly like an agent step's, so
//! the container exits when the command does.
//!
//! The headless path's contract is kept: every byte the container prints is
//! also buffered, and handed back as the step's [`ExecOutput`] so a failing
//! step's `on_failure` agent still gets the full output in its failure file.
//! A PTY merges the command's stdout and stderr into one stream, so the whole
//! transcript is returned as `stdout` and `stderr` is left empty.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::ExecWorkflowCommandFrontend;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::workflow_state::PhaseKind;
use crate::engine::agent_runtime::background::{AgentExec, ExecOutput};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::container::options::{
    ContainerOption, Entrypoint, EnvLiteral, EnvVar, ImageRef, OverlayPermission, OverlaySpec,
    ResolvedContainerOptions,
};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;

type SharedFrontend = Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>;

/// Startup grace for a phase step's container: effectively none. An agent
/// that prints nothing for 30s has failed to start, but a shell command such
/// as `sleep 60` or a quiet `npm ci` is silent by design, and killing it would
/// fail a healthy step.
const PHASE_STEP_GRACE_TIMEOUT: Duration = Duration::MAX;

/// How long to wait, after the container exits, for the output taps to flush
/// the last bytes into the capture buffer. The taps end on their own once the
/// I/O bridge drops its senders; this only bounds a bridge that never does.
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// One setup/teardown step's container, not yet started. The engine asks the
/// phase's container factory for one of these per step, then calls
/// [`AgentExec::exec_streaming`] with the step's command, which launches the
/// container, blocks until it exits, and returns the captured output.
///
/// Carries the same image, workspace mount, overlays and `env()` passthrough
/// the headless path hands `ContainerRuntime::start_background`.
pub(crate) struct InteractivePhaseContainer {
    runtime: Arc<ContainerRuntime>,
    frontend: SharedFrontend,
    kind: PhaseKind,
    image: String,
    workdir: PathBuf,
    env: HashMap<String, String>,
    overlays: Vec<OverlaySpec>,
}

impl InteractivePhaseContainer {
    pub(crate) fn new(
        runtime: Arc<ContainerRuntime>,
        frontend: SharedFrontend,
        kind: PhaseKind,
        image: &str,
        workdir: PathBuf,
        env: HashMap<String, String>,
        overlays: Vec<OverlaySpec>,
    ) -> Self {
        Self {
            runtime,
            frontend,
            kind,
            image: image.to_string(),
            workdir,
            env,
            overlays,
        }
    }

    fn run(
        &self,
        command: &str,
        step_env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        let options = phase_step_container_options(
            &self.image,
            &self.workdir,
            &self.overlays,
            &self.env,
            command,
            step_env,
        );
        let instance = self
            .runtime
            .build(ResolvedContainerOptions::resolve(options)?)?;

        self.frontend
            .lock()
            .unwrap()
            .report_phase_step_interactive_launch(self.kind);

        let capture = OutputCapture::default();
        let proxy = PhaseStepFrontendProxy {
            frontend: Arc::clone(&self.frontend),
            capture: capture.clone(),
        };
        let mut execution = match instance.run_with_frontend(Box::new(proxy)) {
            Ok(e) => e,
            Err(e) => {
                // `take_io` may already have put the terminal in raw mode (CLI)
                // or readied a container window (TUI); hand it back.
                self.frontend
                    .lock()
                    .unwrap()
                    .report_phase_step_container_exited(self.kind, -1);
                return Err(e);
            }
        };

        let handle = tokio::runtime::Handle::current();
        let exit = handle.block_on(async {
            let exit = execution.wait().await;
            drop(execution);
            capture.drain(CAPTURE_DRAIN_TIMEOUT).await;
            exit
        });

        let exit_code = exit.as_ref().map(|e| e.exit_code).unwrap_or(-1);
        self.frontend
            .lock()
            .unwrap()
            .report_phase_step_container_exited(self.kind, exit_code);

        let exit = exit?;
        Ok(ExecOutput {
            stdout: capture.text(),
            stderr: String::new(),
            exit_code: exit.exit_code,
        })
    }
}

impl AgentExec for InteractivePhaseContainer {
    fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.run(command, env)
    }

    /// The output is on the user's screen already, in the PTY, so nothing is
    /// streamed line by line — `on_line` would only print it a second time.
    fn exec_streaming(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
        _on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        self.run(command, env)
    }
}

/// The container options for one interactive phase step.
///
/// Mirrors `build_start_background_argv`: the workspace is mounted read-write
/// at its own path and is the working directory, and the phase's overlays are
/// mounted as resolved. Two differences, both deliberate:
///
/// - The entrypoint is `sh -c <command>`, not `sleep infinity`, so the step's
///   command is the only thing the container runs.
/// - It is interactive, so the frontend's PTY is attached.
///
/// `env()` values stay out of argv: they are passed through by name and the
/// runtime CLI reads the value from its own environment, the same rule every
/// other container launch follows. Step-declared `env` are literals from the
/// repo's own workflow file and keep the `KEY=VALUE` form the headless
/// `exec` uses.
fn phase_step_container_options(
    image: &str,
    workdir: &std::path::Path,
    overlays: &[OverlaySpec],
    env: &HashMap<String, String>,
    command: &str,
    step_env: Option<&HashMap<String, String>>,
) -> Vec<ContainerOption> {
    let mut options = vec![
        ContainerOption::Image(ImageRef::new(image)),
        ContainerOption::Interactive(true),
        ContainerOption::WorkingDir(workdir.to_path_buf()),
        ContainerOption::Overlay(OverlaySpec {
            host_path: workdir.to_path_buf(),
            container_path: workdir.to_path_buf(),
            permission: OverlayPermission::ReadWrite,
        }),
    ];
    options.extend(overlays.iter().cloned().map(ContainerOption::Overlay));

    let mut names: Vec<&String> = env.keys().collect();
    names.sort();
    options.extend(
        names
            .into_iter()
            .map(|name| ContainerOption::EnvPassthrough(EnvVar(name.clone()))),
    );

    if let Some(step_env) = step_env {
        let mut literals: Vec<(&String, &String)> = step_env.iter().collect();
        literals.sort();
        options.extend(literals.into_iter().map(|(key, value)| {
            ContainerOption::EnvLiteral(EnvLiteral {
                key: key.clone(),
                value: value.clone(),
            })
        }));
    }

    options.push(ContainerOption::Entrypoint(Entrypoint::new([
        "sh", "-c", command,
    ])));
    options
}

// ─── Output capture ─────────────────────────────────────────────────────────

/// Everything an interactive phase container prints, buffered alongside the
/// PTY stream the user sees.
#[derive(Clone, Default)]
struct OutputCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
    taps: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl OutputCapture {
    /// Interpose on one of the container's output channels: every chunk sent
    /// to the returned sender is recorded, then forwarded to `sink`.
    fn tap(
        &self,
        sink: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> tokio::sync::mpsc::UnboundedSender<Vec<u8>> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let bytes = Arc::clone(&self.bytes);
        let task = tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if let Ok(mut buf) = bytes.lock() {
                    buf.extend_from_slice(&chunk);
                }
                // A closed sink (the frontend stopped drawing) must not stop
                // the capture: the failure file still needs the output.
                let _ = sink.send(chunk);
            }
        });
        if let Ok(mut taps) = self.taps.lock() {
            taps.push(task);
        }
        tx
    }

    /// Wait (up to `limit`) for every tap to forward its last chunk.
    async fn drain(&self, limit: Duration) {
        let taps = match self.taps.lock() {
            Ok(mut taps) => std::mem::take(&mut *taps),
            Err(_) => return,
        };
        let _ = tokio::time::timeout(limit, async {
            for task in taps {
                let _ = task.await;
            }
        })
        .await;
    }

    fn text(&self) -> String {
        let bytes = self.bytes.lock().map(|b| b.clone()).unwrap_or_default();
        transcript_text(&bytes)
    }
}

/// Turn raw PTY output into plain text for the failure file: terminal escape
/// sequences are dropped and PTY line endings (`\r\n`, or a lone `\r` from a
/// progress line redrawing itself) become `\n`.
///
/// Line endings are normalised first: the escape stripper discards a bare
/// `\r` as a control character, which would glue a progress line's successive
/// redraws together.
fn transcript_text(bytes: &[u8]) -> String {
    let mut normalised = Vec::with_capacity(bytes.len());
    let mut iter = bytes.iter().peekable();
    while let Some(&b) = iter.next() {
        if b == b'\r' {
            if iter.peek() == Some(&&b'\n') {
                iter.next();
            }
            normalised.push(b'\n');
        } else {
            normalised.push(b);
        }
    }
    String::from_utf8_lossy(&strip_ansi_escapes::strip(&normalised)).into_owned()
}

// ─── Frontend proxy ─────────────────────────────────────────────────────────

/// The `AgentFrontend` an interactive phase container runs against: the
/// command's own frontend, with its output channels tapped into an
/// [`OutputCapture`] and the startup-grace kill turned off.
struct PhaseStepFrontendProxy {
    frontend: SharedFrontend,
    capture: OutputCapture,
}

#[async_trait]
impl AgentFrontend for PhaseStepFrontendProxy {
    fn report_status(&mut self, status: AgentStatus) {
        self.frontend.lock().unwrap().report_status(status);
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        self.frontend.lock().unwrap().report_progress(progress);
    }

    fn take_io(&mut self) -> AgentIo {
        let mut io = self.frontend.lock().unwrap().take_io();
        io.stdout = self.capture.tap(io.stdout);
        io.stderr = self.capture.tap(io.stderr);
        io
    }

    fn grace_timeout(&self) -> Duration {
        PHASE_STEP_GRACE_TIMEOUT
    }

    fn stuck_timeout(&self) -> Duration {
        self.frontend.lock().unwrap().stuck_timeout()
    }
}

impl UserMessageSink for PhaseStepFrontendProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.frontend.lock().unwrap().write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.frontend.lock().unwrap().replay_queued();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(options: Vec<ContainerOption>) -> ResolvedContainerOptions {
        ResolvedContainerOptions::resolve(options).expect("options resolve")
    }

    #[test]
    fn the_command_is_the_containers_only_process() {
        let opts = resolve(phase_step_container_options(
            "awman-proj:latest",
            std::path::Path::new("/work/repo"),
            &[],
            &HashMap::new(),
            "make setup && echo done",
            None,
        ));
        assert_eq!(
            opts.entrypoint.as_ref().map(|e| e.0.clone()),
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "make setup && echo done".to_string()
            ])
        );
        assert!(opts.interactive, "a phase step container must get a PTY");
        assert!(
            opts.remove_on_exit,
            "the container must not outlive its command"
        );
        assert!(opts.seeded_prompt.is_none());
    }

    #[test]
    fn the_workspace_is_mounted_read_write_and_is_the_working_directory() {
        let workdir = std::path::Path::new("/work/repo");
        let extra = OverlaySpec {
            host_path: PathBuf::from("/home/u/.ssh"),
            container_path: PathBuf::from("/root/.ssh"),
            permission: OverlayPermission::ReadOnly,
        };
        let opts = resolve(phase_step_container_options(
            "img",
            workdir,
            std::slice::from_ref(&extra),
            &HashMap::new(),
            "true",
            None,
        ));
        assert_eq!(opts.working_dir.as_deref(), Some(workdir));
        assert_eq!(
            opts.overlays,
            vec![
                OverlaySpec {
                    host_path: workdir.to_path_buf(),
                    container_path: workdir.to_path_buf(),
                    permission: OverlayPermission::ReadWrite,
                },
                extra,
            ]
        );
    }

    #[test]
    fn env_overlay_values_are_passed_through_by_name_never_as_literals() {
        let env = HashMap::from([("GITHUB_TOKEN".to_string(), "s3cret".to_string())]);
        let opts = resolve(phase_step_container_options(
            "img",
            std::path::Path::new("/w"),
            &[],
            &env,
            "true",
            None,
        ));
        assert_eq!(opts.env_passthrough, vec![EnvVar("GITHUB_TOKEN".into())]);
        assert!(
            opts.env_literal.iter().all(|l| !l.value.contains("s3cret")),
            "a host env() value must never become a KEY=VALUE literal"
        );
    }

    #[test]
    fn step_declared_env_becomes_literals() {
        let step_env = HashMap::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]);
        let opts = resolve(phase_step_container_options(
            "img",
            std::path::Path::new("/w"),
            &[],
            &HashMap::new(),
            "true",
            Some(&step_env),
        ));
        let literals: Vec<(String, String)> = opts
            .env_literal
            .iter()
            .map(|l| (l.key.clone(), l.value.clone()))
            .collect();
        assert_eq!(
            literals,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn transcript_text_strips_escapes_and_normalises_pty_line_endings() {
        let raw = b"\x1b[32mok\x1b[0m\r\nstep 1\rstep 2\r\ndone";
        assert_eq!(transcript_text(raw), "ok\nstep 1\nstep 2\ndone");
    }

    #[test]
    fn transcript_text_of_nothing_is_empty() {
        assert_eq!(transcript_text(b""), "");
    }

    #[tokio::test]
    async fn a_tap_records_every_chunk_and_still_forwards_it() {
        let capture = OutputCapture::default();
        let (sink, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let tapped = capture.tap(sink);
        tapped.send(b"hello ".to_vec()).unwrap();
        tapped.send(b"world\r\n".to_vec()).unwrap();
        drop(tapped);
        capture.drain(Duration::from_secs(5)).await;

        assert_eq!(capture.text(), "hello world\n");
        assert_eq!(sink_rx.recv().await.unwrap(), b"hello ".to_vec());
        assert_eq!(sink_rx.recv().await.unwrap(), b"world\r\n".to_vec());
    }

    #[tokio::test]
    async fn a_closed_sink_does_not_stop_the_capture() {
        let capture = OutputCapture::default();
        let (sink, sink_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        drop(sink_rx);
        let tapped = capture.tap(sink);
        tapped.send(b"first\n".to_vec()).unwrap();
        tapped.send(b"second\n".to_vec()).unwrap();
        drop(tapped);
        capture.drain(Duration::from_secs(5)).await;

        assert_eq!(capture.text(), "first\nsecond\n");
    }

    #[tokio::test]
    async fn stdout_and_stderr_taps_share_one_transcript() {
        let capture = OutputCapture::default();
        let (sink, _sink_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let out = capture.tap(sink.clone());
        out.send(b"out\n".to_vec()).unwrap();
        drop(out);
        capture.drain(Duration::from_secs(5)).await;
        let err = capture.tap(sink);
        err.send(b"err\n".to_vec()).unwrap();
        drop(err);
        capture.drain(Duration::from_secs(5)).await;

        assert_eq!(capture.text(), "out\nerr\n");
    }
}
