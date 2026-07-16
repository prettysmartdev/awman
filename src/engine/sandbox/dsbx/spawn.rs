//! Single choke-point for every non-interactive `sbx` subprocess awman runs.
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
use std::process::{Command, Stdio};

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
            args: args.into_iter().map(Into::into).collect(),
            stdin: None,
            announce_suffix: None,
            redactions: Vec::new(),
        }
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

    /// Like [`run_quiet`](Self::run_quiet), but a non-zero exit is an error
    /// carrying everything sbx printed (stderr, then stdout). For launch-path
    /// callers that have no frontend sink (`start_sandbox`,
    /// `restart_sandbox`) — the caller surfaces the error text, so sbx's own
    /// diagnostics must travel inside it.
    pub fn run_checked(&self) -> Result<SbxOutput, EngineError> {
        let out = self.run_quiet()?;
        if !out.success() {
            let mut detail = out.stderr.trim().to_string();
            let stdout = out.stdout.trim();
            if !stdout.is_empty() {
                if !detail.is_empty() {
                    detail.push_str("; stdout: ");
                }
                detail.push_str(stdout);
            }
            return Err(EngineError::Sandbox(format!(
                "`{}` exited with code {}: {}",
                self.display_line(),
                out.exit_code,
                redact(&detail, &self.redactions),
            )));
        }
        Ok(out)
    }

    /// Run the command without sink reporting — for the trait methods
    /// (`list_running`, `stats`) that have no frontend to report through, and
    /// for availability probing.
    pub fn run_quiet(&self) -> Result<SbxOutput, EngineError> {
        let mut cmd = Command::new(SBX_BIN);
        cmd.args(&self.args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

    // ─── Shared test helper ───────────────────────────────────────────────

    #[derive(Default)]
    struct VecSink {
        pub messages: Vec<UserMessage>,
    }
    impl UserMessageSink for VecSink {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {}
    }

    // ─── display_line ─────────────────────────────────────────────────────

    #[test]
    fn display_line_includes_suffix() {
        let cmd = SbxCommand::new(["secret", "set", "awman-x-claude", "anthropic"])
            .announce_suffix("(value piped via stdin)");
        assert_eq!(
            cmd.display_line(),
            "sbx secret set awman-x-claude anthropic (value piped via stdin)"
        );
    }

    #[test]
    fn display_line_without_suffix() {
        let cmd = SbxCommand::new(["ls", "--json"]);
        assert_eq!(cmd.display_line(), "sbx ls --json");
    }

    // ─── redact ───────────────────────────────────────────────────────────

    #[test]
    fn redact_masks_secret_value() {
        let masked = redact("token=supersecret done", &["supersecret".to_string()]);
        assert_eq!(masked, "token=*** done");
        assert!(!masked.contains("supersecret"));
    }

    #[test]
    fn redact_noop_without_redactions() {
        assert_eq!(redact("hello world", &[]), "hello world");
    }

    #[test]
    fn redact_masks_multiple_occurrences() {
        let s = redact("key=abc key=abc again", &["abc".to_string()]);
        assert!(!s.contains("abc"));
        assert_eq!(s, "key=*** key=*** again");
    }

    #[test]
    fn redact_skips_empty_string_in_list() {
        // Empty string as a redaction would replace every "" with ***, i.e. every
        // char boundary — that would corrupt the output. The guard prevents this.
        let s = redact("hello", &["".to_string()]);
        assert_eq!(s, "hello");
    }

    // ─── announce ────────────────────────────────────────────────────────

    #[test]
    fn announce_writes_info_level_with_running_prefix() {
        let mut sink = VecSink::default();
        announce(&mut sink, "sbx run --kit /foo/bar claude");
        assert_eq!(sink.messages.len(), 1);
        let msg = &sink.messages[0];
        assert_eq!(msg.level, MessageLevel::Info);
        assert_eq!(msg.text, "Running: sbx run --kit /foo/bar claude");
    }

    // ─── Subprocess announcement ordering ─────────────────────────────────
    //
    // `run_announced` calls `announce()` before `run_quiet()`, so even when
    // `sbx` is not installed the Info announcement has already been written to
    // the sink by the time the error is returned.

    #[test]
    fn run_announced_writes_info_announcement_before_subprocess() {
        let mut sink = VecSink::default();
        let cmd = SbxCommand::new(["version"]);
        // Ignore the result — sbx may not be installed.
        let _ = cmd.run_announced(&mut sink);
        assert!(
            sink.messages
                .iter()
                .any(|m| { m.level == MessageLevel::Info && m.text == "Running: sbx version" }),
            "Info announcement must be written even when sbx is unavailable; messages: {:?}",
            sink.messages
        );
    }

    #[test]
    fn run_announced_announcement_never_contains_value_when_suffix_used() {
        let mut sink = VecSink::default();
        let cmd = SbxCommand::new(["secret", "set", "awman-x-claude", "anthropic"])
            .announce_suffix("(value piped via stdin)")
            .with_stdin(b"sk-supersecret".to_vec())
            .redact("sk-supersecret".to_string());
        let _ = cmd.run_announced(&mut sink);
        // The announcement line must NOT contain the secret value.
        for msg in &sink.messages {
            assert!(
                !msg.text.contains("sk-supersecret"),
                "secret must not appear in any sink message: {:?}",
                msg.text
            );
        }
    }

    // ─── Subprocess output routing (requires a fake `sbx` on PATH) ────────
    //
    // These tests prepend a temp dir containing a mock `sbx` script to PATH
    // (via the dsbx-wide `test_support::with_fake_sbx`, whose lock serialises
    // PATH mutations across parallel test threads), then invoke SbxCommand
    // and verify stdout→Info and stderr→Warning routing.

    #[cfg(unix)]
    mod subprocess_routing {
        use super::*;
        use crate::engine::sandbox::dsbx::test_support::with_fake_sbx;

        #[test]
        fn stdout_lines_arrive_at_info_level() {
            with_fake_sbx("#!/bin/sh\necho 'hello stdout'\n", || {
                let mut sink = VecSink::default();
                SbxCommand::new(["dummy"]).run_announced(&mut sink).unwrap();
                assert!(
                    sink.messages
                        .iter()
                        .any(|m| { m.level == MessageLevel::Info && m.text == "hello stdout" }),
                    "stdout must arrive at Info level; messages: {:?}",
                    sink.messages
                );
            });
        }

        #[test]
        fn stderr_lines_arrive_at_warning_level_on_success() {
            with_fake_sbx("#!/bin/sh\necho 'side note' >&2\n", || {
                let mut sink = VecSink::default();
                SbxCommand::new(["dummy"]).run_announced(&mut sink).unwrap();
                assert!(
                    sink.messages
                        .iter()
                        .any(|m| { m.level == MessageLevel::Warning && m.text == "side note" }),
                    "stderr on a successful run must arrive at Warning; messages: {:?}",
                    sink.messages
                );
            });
        }

        #[test]
        fn non_zero_exit_returns_sandbox_error_with_argv_and_stderr() {
            with_fake_sbx("#!/bin/sh\necho 'oops' >&2\nexit 2\n", || {
                let mut sink = VecSink::default();
                let result = SbxCommand::new(["broken"]).run_announced(&mut sink);
                match result {
                    Err(EngineError::Sandbox(msg)) => {
                        assert!(msg.contains("sbx broken"), "error must contain argv: {msg}");
                        assert!(msg.contains("oops"), "error must contain stderr: {msg}");
                        assert!(msg.contains("2"), "error must contain exit code: {msg}");
                    }
                    other => panic!("expected Sandbox error, got: {other:?}"),
                }
            });
        }

        // ─── run_checked ───────────────────────────────────────────────────

        #[test]
        fn run_checked_error_carries_stderr_and_stdout() {
            with_fake_sbx(
                "#!/bin/sh\necho 'out detail'\necho 'ERROR: failed to create sandbox: boom' >&2\nexit 1\n",
                || {
                    let err = SbxCommand::new(["create"]).run_checked().unwrap_err();
                    match err {
                        EngineError::Sandbox(msg) => {
                            assert!(msg.contains("sbx create"), "must name the argv: {msg}");
                            assert!(msg.contains("code 1"), "must name the exit code: {msg}");
                            assert!(
                                msg.contains("failed to create sandbox: boom"),
                                "must carry sbx's stderr: {msg}"
                            );
                            assert!(msg.contains("out detail"), "must carry sbx's stdout: {msg}");
                        }
                        other => panic!("expected Sandbox error, got: {other:?}"),
                    }
                },
            );
        }

        #[test]
        fn run_checked_passes_through_success() {
            with_fake_sbx("#!/bin/sh\necho ok\n", || {
                let out = SbxCommand::new(["create"]).run_checked().unwrap();
                assert_eq!(out.exit_code, 0);
                assert_eq!(out.stdout.trim(), "ok");
            });
        }

        #[test]
        fn secret_value_redacted_from_sink_output() {
            // A fake sbx that echoes its stdin back on stdout (simulating a
            // poorly-behaved tool that leaks its input). The .redact() call must
            // mask the value before it reaches the sink.
            with_fake_sbx("#!/bin/sh\ncat\n", || {
                let mut sink = VecSink::default();
                let _ = SbxCommand::new(["secret", "set", "awman-x-claude", "anthropic"])
                    .with_stdin(b"sk-supersecret".to_vec())
                    .announce_suffix("(value piped via stdin)")
                    .redact("sk-supersecret".to_string())
                    .run_announced(&mut sink);
                for msg in &sink.messages {
                    assert!(
                        !msg.text.contains("sk-supersecret"),
                        "secret leaked in sink message: {:?}",
                        msg.text
                    );
                }
            });
        }
    }
}
