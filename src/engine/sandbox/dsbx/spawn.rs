//! Single choke-point for every headless `sbx` subprocess awman runs.
//!
//! Implements the "Subprocess transparency via the user message sink"
//! requirement from WI 0090: announce the argv before spawning, publish
//! stdout/stderr, and report non-zero exits with the command and captured
//! stderr wrapped in [`EngineError::Sandbox`]. Secret values are piped via
//! stdin (never argv) and redacted from any published output.
//! Readiness probes suppress successful stdout and announcements; their
//! warnings and failure diagnostics remain visible.
//!
//! The interactive `sbx run` PTY session does NOT flow through here — it is
//! bridged by [`super::io_bridge`] — but its argv announcement still uses
//! [`announce`].

use std::io::Write;
use std::process::Stdio;

use crate::data::message::UserMessageSink;
use crate::engine::error::EngineError;

/// The CLI binary every sandbox subprocess drives.
pub(super) const SBX_BIN: &str = "sbx";

/// Captured result of a finished `sbx` subprocess.
#[derive(Debug, Clone)]
pub(super) struct SbxOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl SbxOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// A described `sbx` invocation, ready to run with full sink transparency.
pub(super) struct SbxCommand {
    env: Option<crate::data::config::env::EnvSnapshot>,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
    /// Appended to the announcement instead of revealing piped stdin
    /// (e.g. "(value piped via stdin)").
    announce_suffix: Option<String>,
    /// Sensitive substrings redacted from any published stdout/stderr.
    redactions: Vec<String>,
    /// Kill the child and fail when it runs longer than this.
    deadline: Option<std::time::Duration>,
}

impl SbxCommand {
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            env: None,
            args: args.into_iter().map(Into::into).collect(),
            stdin: None,
            announce_suffix: None,
            redactions: Vec::new(),
            deadline: None,
        }
    }

    pub fn in_env(mut self, env: &crate::data::config::env::EnvSnapshot) -> Self {
        self.env = Some(env.clone());
        self
    }

    pub fn with_stdin(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
        self
    }

    pub fn announce_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.announce_suffix = Some(suffix.into());
        self
    }

    /// Bound the run: the child is killed and the run fails after `limit`.
    pub fn with_deadline(mut self, limit: std::time::Duration) -> Self {
        self.deadline = Some(limit);
        self
    }

    pub fn redact(mut self, value: impl Into<String>) -> Self {
        let v = value.into();
        if !v.is_empty() {
            self.redactions.push(v);
        }
        self
    }

    /// The human-readable command line, with the announce suffix appended.
    fn display_line(&self) -> String {
        let mut line = format!("{SBX_BIN} {}", self.args.join(" "));
        if let Some(suffix) = &self.announce_suffix {
            line.push(' ');
            line.push_str(suffix);
        }
        line
    }

    /// Run the command, announcing the argv and publishing output on `sink`.
    pub fn run_announced(&self, sink: &mut dyn UserMessageSink) -> Result<SbxOutput, EngineError> {
        announce(sink, &self.display_line());
        let out = self.run_quiet()?;
        for line in redact(&out.stdout, &self.redactions).lines() {
            sink.write_message(info(redact(line, &self.redactions)));
        }
        let stderr_level = if out.success() {
            crate::data::message::MessageLevel::Warning
        } else {
            crate::data::message::MessageLevel::Error
        };
        for line in redact(&out.stderr, &self.redactions).lines() {
            sink.write_message(crate::data::message::UserMessage {
                level: stderr_level,
                text: redact(line, &self.redactions),
            });
        }
        if !out.success() {
            return Err(EngineError::Sandbox(format!(
                "`{}` exited with code {}: {}",
                self.display_line(),
                out.exit_code,
                redact(&out.stderr, &self.redactions).trim(),
            )));
        }
        Ok(out)
    }

    /// Readiness probes are quiet on success. Preserve warnings and include
    /// the command and captured diagnostics on failure for the caller to report.
    pub fn check_preparation(&self, sink: &mut dyn UserMessageSink) -> Result<(), EngineError> {
        let out = self.run_quiet()?;
        self.report_preparation(out, sink)
    }

    fn report_preparation(
        &self,
        out: SbxOutput,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        if !out.success() {
            let detail = format!(
                "{}\n{}",
                redact(&out.stdout, &self.redactions).trim(),
                redact(&out.stderr, &self.redactions).trim()
            );
            return Err(EngineError::Sandbox(format!(
                "`{}` exited with code {}: {}",
                self.display_line(),
                out.exit_code,
                redact(detail.trim(), &self.redactions),
            )));
        }
        for line in out.stderr.lines() {
            sink.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: redact(line, &self.redactions),
            });
        }
        Ok(())
    }

    /// The `sbx` subcommand: the first argument that is not a global flag
    /// such as `--no-input`.
    fn subcommand(&self) -> Option<&str> {
        self.args
            .iter()
            .map(String::as_str)
            .find(|arg| !arg.starts_with('-'))
    }

    /// Run the command without sink reporting — for the trait methods
    /// (`list_running`, `stats`) that have no frontend to report through, and
    /// for availability probing.
    pub fn run_quiet(&self) -> Result<SbxOutput, EngineError> {
        let snapshot = self
            .env
            .as_ref()
            .map(std::borrow::Cow::Borrowed)
            .unwrap_or_else(|| std::borrow::Cow::Owned(crate::engine::host_cli::process_env()));
        let mut cmd = crate::engine::host_cli::command(&snapshot, SBX_BIN);
        cmd.args(&self.args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        #[cfg(unix)]
        if self.deadline.is_some() {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        // Runtime observation must be bounded and cancellable in status watches.
        if self.deadline.is_none() && self.subcommand() == Some("ls") {
            let output = crate::engine::host_cli::checked_output(&mut cmd, SBX_BIN)?;
            return Ok(SbxOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            });
        }
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::Sandbox(SBX_NOT_FOUND.to_string())
            } else {
                EngineError::Sandbox(format!("spawn `{}`: {e}", self.display_line()))
            }
        })?;

        let Some(deadline) = self.deadline else {
            if let Some(bytes) = &self.stdin {
                if let Some(stdin) = child.stdin.take() {
                    write_child_stdin(stdin, bytes)?;
                }
            }
            let output = child.wait_with_output().map_err(|e| {
                EngineError::Sandbox(format!("wait `{}`: {e}", self.display_line()))
            })?;
            return Ok(SbxOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            });
        };
        self.wait_with_deadline(child, deadline)
    }

    /// Wait for `child` until `deadline`, killing it when the deadline
    /// passes. Stdin is written and stdout/stderr are drained on their own
    /// threads so a child that stops reading cannot stall the deadline.
    fn wait_with_deadline(
        &self,
        mut child: std::process::Child,
        deadline: std::time::Duration,
    ) -> Result<SbxOutput, EngineError> {
        use std::io::Read;
        let writer = match (&self.stdin, child.stdin.take()) {
            (Some(bytes), Some(stdin)) => {
                let bytes = bytes.clone();
                Some(std::thread::spawn(move || write_child_stdin(stdin, &bytes)))
            }
            _ => None,
        };
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.by_ref().take(1024 * 1024).read_to_end(&mut buf);
                }
                buf
            })
        };
        let stdout = drain(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let stderr = drain(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let started = std::time::Instant::now();
        let status = loop {
            // Do not reap the group leader while a descendant holds our pipes.
            // Keeping its PID reserved prevents group-ID reuse before timeout
            // cleanup. Reap only when every I/O worker has completed.
            if writer.as_ref().is_none_or(|w| w.is_finished())
                && stdout.is_finished()
                && stderr.is_finished()
            {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {}
                    Err(error) => {
                        #[cfg(unix)]
                        let _ = nix::sys::signal::killpg(
                            nix::unistd::Pid::from_raw(child.id() as i32),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(EngineError::Sandbox(format!("wait sbx: {error}")));
                    }
                }
            }
            if started.elapsed() >= deadline {
                #[cfg(unix)]
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let _ = child.kill();
                let _ = child.wait();
                return Err(EngineError::Sandbox(format!(
                    "{} did not finish within {} s; local client group stopped; remote apply completion is unknown",
                    self.display_line(), deadline.as_secs()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        if let Some(writer) = writer {
            writer
                .join()
                .map_err(|_| EngineError::Sandbox("sbx stdin writer panicked".into()))??;
        }
        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        Ok(SbxOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

/// The error for a missing `sbx` executable.
pub(super) const SBX_NOT_FOUND: &str =
    "`sbx` binary not found on PATH; install Docker Sandboxes (`brew install docker/tap/sbx`)";

/// Write `bytes` to a child's stdin and close it.
///
/// The child can exit before it finishes reading stdin — e.g. an `sbx secret
/// set` that bails with "not logged in" and never consumes the piped secret.
/// That closes the read end and the write surfaces as BrokenPipe. It is not
/// our error to report: the child's exit code and stderr are authoritative.
fn write_child_stdin(mut stdin: std::process::ChildStdin, bytes: &[u8]) -> Result<(), EngineError> {
    if let Err(e) = stdin.write_all(bytes) {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(EngineError::Sandbox(format!("write sbx stdin: {e}")));
        }
    }
    // Drop closes the pipe so the child sees EOF.
    Ok(())
}

/// Announce a command line on the sink at Info level, before it runs.
pub(super) fn announce(sink: &mut dyn UserMessageSink, line: &str) {
    sink.write_message(info(format!("Running: {line}")));
}

fn info(text: impl Into<String>) -> crate::data::message::UserMessage {
    crate::data::message::UserMessage {
        level: crate::data::message::MessageLevel::Info,
        text: text.into(),
    }
}

/// Replace every sensitive substring with `***`.
fn redact(line: &str, redactions: &[String]) -> String {
    let mut out = line.to_string();
    for r in redactions {
        if !r.is_empty() {
            out = out.replace(r, "***");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::{MessageLevel, RecordingMessageSink};

    #[test]
    fn preparation_probes_are_quiet_but_preserve_warnings_and_failure_details() {
        let mut command = SbxCommand::new(["kit", "validate", "/private/kit"]);
        command.redactions.push("secret-value".into());
        let mut sink = RecordingMessageSink::new();
        command
            .report_preparation(
                SbxOutput {
                    stdout: "kit validated\nverbose source metadata\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
                &mut sink,
            )
            .unwrap();
        assert!(sink.all().is_empty());
        command
            .report_preparation(
                SbxOutput {
                    stdout: "kit validated".into(),
                    stderr: "deprecated field: secret-value".into(),
                    exit_code: 0,
                },
                &mut sink,
            )
            .unwrap();
        assert_eq!(sink.all().len(), 1);
        assert_eq!(sink.all()[0].level, MessageLevel::Warning);
        assert_eq!(sink.all()[0].text, "deprecated field: ***");
        let error = command
            .report_preparation(
                SbxOutput {
                    stdout: "invalid kit field".into(),
                    stderr: "validation failed: secret-value".into(),
                    exit_code: 2,
                },
                &mut sink,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("sbx kit validate /private/kit"));
        assert!(error.contains("exited with code 2"));
        assert!(error.contains("invalid kit field"));
        assert!(error.contains("validation failed: ***"));
        assert!(!error.contains("secret-value"));
    }
}
