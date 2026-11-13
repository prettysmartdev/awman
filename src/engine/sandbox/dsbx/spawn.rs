//! Single choke-point for every headless `sbx` subprocess awman runs.
//!
//! Implements the "Subprocess transparency via the user message sink"
//! requirement from WI 0090: announce the argv before spawning, publish
//! stdout/stderr, and report non-zero exits with the command and captured
//! stderr wrapped in [`EngineError::Sandbox`]. Secret values are piped via
//! stdin (never argv) and redacted from any published output.
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
        for line in out.stdout.lines() {
            sink.write_message(info(redact(line, &self.redactions)));
        }
        let stderr_level = if out.success() {
            crate::data::message::MessageLevel::Warning
        } else {
            crate::data::message::MessageLevel::Error
        };
        for line in out.stderr.lines() {
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
                redact(out.stderr.trim(), &self.redactions),
            )));
        }
        Ok(out)
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

        // Runtime observation must be bounded and cancellable in status watches.
        if self.args.first().is_some_and(|arg| arg == "ls") {
            let output = crate::engine::host_cli::checked_output(&mut cmd, SBX_BIN)?;
            return Ok(SbxOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            });
        }
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::Sandbox(
                    "`sbx` binary not found on PATH; install Docker Sandboxes \
                     (`brew install docker/tap/sbx`)"
                        .to_string(),
                )
            } else {
                EngineError::Sandbox(format!("spawn `{}`: {e}", self.display_line()))
            }
        })?;

        if let Some(bytes) = &self.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                // The child can exit before it finishes reading stdin — e.g. an
                // `sbx secret set` that bails with "not logged in" and never
                // consumes the piped secret. That closes the read end and the
                // write surfaces as BrokenPipe. It is not our error to report:
                // the child's exit code and stderr are authoritative, so swallow
                // it and let `wait_with_output` below surface the real failure.
                if let Err(e) = stdin.write_all(bytes) {
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(EngineError::Sandbox(format!("write sbx stdin: {e}")));
                    }
                }
                // Drop closes the pipe so the child sees EOF.
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|e| EngineError::Sandbox(format!("wait `{}`: {e}", self.display_line())))?;
        Ok(SbxOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
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
