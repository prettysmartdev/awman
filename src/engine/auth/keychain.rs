//! Per-platform keychain credential resolution.
//!
//! macOS uses `security find-generic-password`. Linux uses `secret-tool`
//! (libsecret/Secret-Service) when available. Windows returns no credentials.
//!
//! Two delivery shapes, picked per agent:
//!
//! 1. **Env-var credentials** (`agent_keychain_credentials`) — `(key, value)`
//!    pairs injected via `docker -e` / `container --env`. Used by agents that
//!    accept their OAuth token through an env var (e.g. Claude with
//!    `CLAUDE_CODE_OAUTH_TOKEN`).
//!
//! 2. **File-form credentials** (`agent_keychain_files`) — files to plant
//!    inside the agent's settings-dir overlay before mount. Used by agents
//!    that only read tokens from a fixed on-disk path (e.g. Antigravity
//!    reads `~/.gemini/antigravity-cli/antigravity-oauth-token` when its
//!    in-container keyring is unreachable).

use std::path::PathBuf;
use std::process::Command;

use crate::data::session::AgentName;
use crate::engine::agent::agent_matrix::CredentialSource;
use crate::engine::auth::credential::{
    self, HostCredentialSource, RefreshableCredentialSpec, CLAUDE_KEYCHAIN_SERVICE,
};

/// File-form credential to be written into an agent's settings-dir overlay
/// before mounting the overlay into the container. Lifecycle: produced from
/// the host keychain, copied into a per-session tempdir alongside the rest of
/// the agent's settings, then bind-mounted under the agent's `$HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSecretFile {
    /// Path **relative** to the agent's settings dir. E.g. for antigravity
    /// this is `antigravity-cli/antigravity-oauth-token`, joined with the
    /// staged `~/.gemini` to produce the final on-disk path.
    pub relative_path: PathBuf,
    /// File contents.
    pub contents: Vec<u8>,
    /// Unix permission mode (e.g. `0o600`). Ignored on non-Unix.
    pub mode: u32,
}

/// Env-var credentials for the agent. Empty when the platform has no keychain
/// integration, the entry is missing, or the payload fails to decode.
pub fn agent_keychain_credentials(agent: &AgentName) -> Vec<(String, String)> {
    match credential_source(agent) {
        CredentialSource::ClaudeKeychainOauth => claude_keychain_credentials(),
        CredentialSource::AntigravityKeychainFile | CredentialSource::None => Vec::new(),
    }
}

/// This agent's credential scheme, from the one per-agent table
/// (`agent_matrix`). An agent the matrix does not know has no host credential
/// awman passes through — which is exactly what the `_ =>` arms these three
/// functions used to carry said.
fn credential_source(agent: &AgentName) -> CredentialSource {
    crate::engine::agent::agent_matrix::matrix_for(agent.as_str())
        .map(|m| m.credential_source)
        .unwrap_or(CredentialSource::None)
}

/// File-form credentials for the agent. Empty when the platform has no
/// keychain integration, the entry is missing, or the payload fails to decode.
pub fn agent_keychain_files(agent: &AgentName) -> Vec<AgentSecretFile> {
    match credential_source(agent) {
        CredentialSource::AntigravityKeychainFile => antigravity_keychain_files(),
        CredentialSource::ClaudeKeychainOauth | CredentialSource::None => Vec::new(),
    }
}

/// Refresh descriptor lookup — the generic replacement for the per-agent
/// dispatch. `Some` only for agents that opt into refreshable file delivery;
/// `None` keeps an agent's behaviour EXACTLY as it is today (env delivery for
/// claude-less agents, `AgentSecretFile` delivery for antigravity).
pub fn refreshable_spec_for(agent: &AgentName) -> Option<&'static RefreshableCredentialSpec> {
    match credential_source(agent) {
        CredentialSource::ClaudeKeychainOauth => Some(credential::claude_spec()),
        CredentialSource::AntigravityKeychainFile | CredentialSource::None => None,
    }
}

// ── Claude (env-var) ────────────────────────────────────────────────────────

/// macOS-only: look up the Claude Code OAuth credential and extract its access
/// token. This is the legacy **env** delivery path (`CLAUDE_CODE_OAUTH_TOKEN`),
/// still used by the sandbox/sbx runtime and any non-container caller; the
/// container path now delivers Claude's credential as a refreshable file via
/// [`refreshable_spec_for`].
///
/// The keychain code stays macOS-only (the descriptor's cross-platform *source*
/// handles non-macOS); a missing entry or unparseable payload is logged with a
/// typed reason so `awman ready` can surface it, rather than silently returning
/// an empty vec.
fn claude_keychain_credentials() -> Vec<(String, String)> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let source = HostCredentialSource::MacosKeychain {
        service: CLAUDE_KEYCHAIN_SERVICE,
    };
    match credential::read_claude_credential(&source) {
        Ok(snap) => vec![(
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            snap.secret.expose().to_string(),
        )],
        Err(reason) => {
            // Never log the secret — `reason` and the agent name only.
            tracing::warn!(
                agent = "claude",
                reason = %reason,
                "could not read Claude keychain credential for env delivery"
            );
            Vec::new()
        }
    }
}

// ── Antigravity (file-form) ─────────────────────────────────────────────────

/// Antigravity stores its OAuth token under macOS Keychain service `gemini`,
/// account `antigravity` (or the corresponding libsecret entry on Linux),
/// wrapped with the `go-keyring-base64:` envelope (zalando/go-keyring on
/// macOS encodes every secret this way to dodge `security`'s hex-mangling).
/// Unwrapped, the payload is a JSON object:
///
/// ```json
/// {"token":{"access_token":"...","token_type":"Bearer",
///           "refresh_token":"...","expiry":"..."},
///  "auth_method":"consumer"}
/// ```
///
/// Inside the container the keyring backend has no D-Bus to talk to, so agy
/// falls back to reading the same JSON from a fixed file at
/// `~/.gemini/antigravity-cli/antigravity-oauth-token` (verified via strace
/// of agy + a live login round-trip).
fn antigravity_keychain_files() -> Vec<AgentSecretFile> {
    let Some(raw) = read_antigravity_secret() else {
        return Vec::new();
    };
    let Some(decoded) = decode_go_keyring_payload(&raw) else {
        return Vec::new();
    };
    // Sanity-check the JSON shape so we never plant a malformed token file
    // that would itself fail the container's silent-auth path.
    let parsed: serde_json::Value = match serde_json::from_slice(&decoded) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if parsed
        .get("token")
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .is_none()
    {
        return Vec::new();
    }
    vec![AgentSecretFile {
        relative_path: PathBuf::from("antigravity-cli").join("antigravity-oauth-token"),
        contents: decoded,
        mode: 0o600,
    }]
}

fn read_antigravity_secret() -> Option<String> {
    if cfg!(target_os = "macos") {
        run_macos_keychain_lookup("gemini", Some("antigravity"))
    } else if cfg!(target_os = "linux") {
        run_linux_secret_lookup("gemini", "antigravity")
    } else {
        None
    }
}

// ── Shared OS keychain shims ────────────────────────────────────────────────

pub(crate) fn run_macos_keychain_lookup(service: &str, account: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("security");
    cmd.arg("find-generic-password").arg("-s").arg(service);
    if let Some(a) = account {
        cmd.arg("-a").arg(a);
    }
    cmd.arg("-w");
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

fn run_linux_secret_lookup(service: &str, account: &str) -> Option<String> {
    let out = Command::new("secret-tool")
        .args(["lookup", "service", service, "account", account])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// Strip the `go-keyring-base64:` envelope (or the legacy
/// `go-keyring-encoded:` hex form) used by zalando/go-keyring. Passes the
/// payload through unchanged when no prefix matches.
pub(crate) fn decode_go_keyring_payload(raw: &str) -> Option<Vec<u8>> {
    const B64_PREFIX: &str = "go-keyring-base64:";
    const HEX_PREFIX: &str = "go-keyring-encoded:";
    if let Some(rest) = raw.strip_prefix(B64_PREFIX) {
        // go-keyring writes a strict RFC 4648 standard-alphabet payload; allow
        // the trailing newlines that some shells leave on the value.
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            rest.trim().as_bytes(),
        )
        .ok()
    } else if let Some(rest) = raw.strip_prefix(HEX_PREFIX) {
        decode_hex(rest.trim())
    } else {
        Some(raw.as_bytes().to_vec())
    }
}

fn decode_hex(input: &str) -> Option<Vec<u8>> {
    if !input.len().is_multiple_of(2) {
        return None;
    }
    (0..input.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(input.get(i..i + 2)?, 16).ok())
        .collect()
}

// ── Capped keychain read/write/clear (WI 0116 §5) ───────────────────────────
//
// The shims above resolve *agent* credentials and are read-only. Squad's
// daemon-env store needs a write and a clear as well, and — unlike an agent
// credential read on an interactive command — it runs inside an unattended
// daemon, where a locked login keychain or an absent Secret Service must never
// be able to hang a start, a push, or a run. Everything below is therefore
// capped ([`run_capped`]) and reports a typed error the caller degrades on.
//
// These stay the only keychain *access* in the tree: no `keyring` or
// `secret-service` crate, which would link Security.framework or
// libsecret/D-Bus and break the single-statically-linked-binary constraint in
// `aspec/architecture/design.md`.

/// Why a capped keychain call did not produce an answer.
#[derive(Debug)]
pub(crate) enum KeychainCallError {
    /// The backend's binary is not installed.
    BinaryMissing(String),
    /// The child did not exit within the cap and was killed.
    Timeout,
    /// The child ran and failed. `stderr` never contains the secret: neither
    /// `security` nor `secret-tool` echoes a value it was handed on stdin.
    Failed { status: Option<i32>, stderr: String },
}

impl std::fmt::Display for KeychainCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeychainCallError::BinaryMissing(bin) => write!(f, "{bin} not found"),
            KeychainCallError::Timeout => write!(f, "timed out after 5s"),
            KeychainCallError::Failed { status, stderr } => {
                let code = status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                if stderr.is_empty() {
                    write!(f, "exit {code}")
                } else {
                    write!(f, "exit {code}: {stderr}")
                }
            }
        }
    }
}

/// Spawn `cmd`, optionally feed it `stdin`, and kill it at `cap`.
///
/// `Command::output()` would block forever on a keychain that never answers,
/// which is the case this exists for. The child is polled instead and killed on
/// expiry, so nothing is left running — the detached-thread cap in
/// `data::fs::daemon_env::call_with_cap` alone would leak the process.
///
/// stdout and stderr are drained on their own threads, started before the wait
/// loop. `find-generic-password -w` prints the whole stored payload, and the
/// daemon's payload is every value it holds — one PEM key or service-account
/// blob puts it past a kernel pipe buffer (64 KiB on Linux). A child whose
/// output fills that buffer cannot exit until someone reads it, so a reader
/// that waits for the exit first deadlocks until the cap kills a backend that
/// was working: the read that reports the item comes back `Timeout`, the daemon
/// degrades, and env persistence is silently lost.
pub(crate) fn run_capped(
    cmd: &mut Command,
    stdin: Option<&[u8]>,
    cap: std::time::Duration,
) -> Result<std::process::Output, KeychainCallError> {
    use std::io::Write;
    use std::process::Stdio;

    let program = cmd.get_program().to_string_lossy().to_string();
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(KeychainCallError::BinaryMissing(program));
        }
        Err(e) => {
            return Err(KeychainCallError::Failed {
                status: None,
                stderr: e.to_string(),
            });
        }
    };

    // The deadline covers the stdin write as well as the wait. `write_all` to a
    // pipe blocks once the kernel buffer (64 KiB on Linux) fills and the child
    // is not draining it, so establishing the deadline afterwards meant the
    // kill-at-expiry path below was simply never reached for a wedged child:
    // the helper thread blocked forever, `call_with_cap` detached it, and both
    // it and the orphaned `security`/`secret-tool` process leaked for the life
    // of the daemon. The write therefore goes to a short-lived thread and the
    // polled loop below owns the deadline unconditionally; killing the child
    // drops its stdin, which unblocks that thread.
    let deadline = std::time::Instant::now() + cap;
    // Started before the wait loop so the child is never blocked writing while
    // this thread is blocked waiting for it to exit. Detached on expiry for the
    // same reason the writer below is: a grandchild holding the pipe must not
    // turn a capped call into an unbounded one.
    let stdout_rx = drain_on_thread(child.stdout.take());
    let stderr_rx = drain_on_thread(child.stderr.take());
    let writer = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => {
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || {
                // A write failure is not fatal on its own: the child's exit
                // status below is what decides the outcome. Dropping the handle
                // closes the pipe, which is what tells `security -i` and
                // `secret-tool` that input is complete.
                let _ = pipe.write_all(&bytes);
            }))
        }
        _ => None,
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(KeychainCallError::Timeout);
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(KeychainCallError::Failed {
                    status: None,
                    stderr: e.to_string(),
                });
            }
        }
    };

    // The writer is deliberately never joined. On the normal path the child has
    // exited, so the write has either finished or taken an `EPIPE` and the
    // thread is already ending. On the timeout path joining would be exactly
    // the bug this change exists to fix: killing the child does not close the
    // pipe if the child left a grandchild holding the read end, and the join
    // would then block for as long as that grandchild lives — reintroducing an
    // unbounded wait inside a call whose whole point is to be capped.
    drop(writer);

    // The child has exited, so both drains are finished or a keystroke away —
    // unless a grandchild still holds a pipe, which is what the remaining
    // budget is for. A drain that does not finish is reported as `Timeout`
    // rather than as empty output: an empty stdout here reads as "the backend
    // holds no such item", and answering that about an item that exists would
    // hand the daemon a cold start and drop every value it had persisted.
    let stdout = collect_drain(stdout_rx, deadline)?;
    let stderr = collect_drain(stderr_rx, deadline)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Read a child pipe to EOF on its own thread, handing the bytes back over a
/// channel so the caller can bound the wait.
fn drain_on_thread<R>(pipe: Option<R>) -> Option<std::sync::mpsc::Receiver<Vec<u8>>>
where
    R: std::io::Read + Send + 'static,
{
    let mut pipe = pipe?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        // A read failure is not fatal on its own, exactly as for the writer:
        // the child's exit status is what decides the outcome.
        let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
        let _ = tx.send(buf);
    });
    Some(rx)
}

/// Take what [`drain_on_thread`] read, waiting no longer than `deadline`.
///
/// A `recv_timeout` of zero still yields a value that already arrived, so a
/// child that exits exactly at the deadline is not failed for it.
fn collect_drain(
    rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, KeychainCallError> {
    let Some(rx) = rx else {
        return Ok(Vec::new());
    };
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    rx.recv_timeout(remaining)
        .map_err(|_| KeychainCallError::Timeout)
}

/// macOS `security`/Linux `secret-tool` exit code for "the item does not
/// exist". Not an error: a daemon that has never stored anything is the normal
/// cold-start state.
const MACOS_ITEM_NOT_FOUND: i32 = 44;

/// Read one generic-password item. `Ok(None)` means the backend works and holds
/// no such item.
pub(crate) fn keychain_lookup(
    service: &str,
    account: &str,
    cap: std::time::Duration,
) -> Result<Option<String>, KeychainCallError> {
    if cfg!(target_os = "macos") {
        let mut cmd = Command::new("security");
        cmd.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
        let out = run_capped(&mut cmd, None, cap)?;
        if out.status.code() == Some(MACOS_ITEM_NOT_FOUND) {
            return Ok(None);
        }
        if !out.status.success() {
            return Err(failed(&out));
        }
        Ok(non_empty(&out.stdout))
    } else if cfg!(target_os = "linux") {
        let mut cmd = Command::new("secret-tool");
        cmd.args(["lookup", "service", service, "account", account]);
        let out = run_capped(&mut cmd, None, cap)?;
        // `secret-tool lookup` reports a miss as a non-zero exit with nothing
        // on stderr, and a working-but-empty item as exit 0 with no stdout.
        if !out.status.success() {
            if out.stderr.is_empty() {
                return Ok(None);
            }
            return Err(failed(&out));
        }
        Ok(non_empty(&out.stdout))
    } else {
        Err(KeychainCallError::BinaryMissing("<none>".to_string()))
    }
}

/// Write one generic-password item, replacing any existing one.
///
/// `envelope` is the already-wrapped value; it never appears in an argv on
/// either platform — `security -i` reads its commands from stdin, and
/// `secret-tool store` reads the secret from stdin by design.
pub(crate) fn keychain_store(
    service: &str,
    account: &str,
    envelope: &str,
    cap: std::time::Duration,
) -> Result<(), KeychainCallError> {
    if cfg!(target_os = "macos") {
        let script = crate::data::fs::daemon_env::security_add_generic_password_script(
            service, account, envelope,
        );
        let mut cmd = Command::new("security");
        cmd.arg("-i");
        let out = run_capped(&mut cmd, Some(script.as_bytes()), cap)?;
        if !out.status.success() {
            return Err(failed(&out));
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        let mut cmd = Command::new("secret-tool");
        cmd.args([
            "store",
            "--label=awman-squad daemon env",
            "service",
            service,
            "account",
            account,
        ]);
        let out = run_capped(&mut cmd, Some(envelope.as_bytes()), cap)?;
        if !out.status.success() {
            return Err(failed(&out));
        }
        Ok(())
    } else {
        Err(KeychainCallError::BinaryMissing("<none>".to_string()))
    }
}

/// Remove the item. Idempotent — "no such item" is `Ok(())`.
pub(crate) fn keychain_clear(
    service: &str,
    account: &str,
    cap: std::time::Duration,
) -> Result<(), KeychainCallError> {
    if cfg!(target_os = "macos") {
        let mut cmd = Command::new("security");
        cmd.args(["delete-generic-password", "-s", service, "-a", account]);
        let out = run_capped(&mut cmd, None, cap)?;
        if out.status.success() || out.status.code() == Some(MACOS_ITEM_NOT_FOUND) {
            return Ok(());
        }
        Err(failed(&out))
    } else if cfg!(target_os = "linux") {
        let mut cmd = Command::new("secret-tool");
        cmd.args(["clear", "service", service, "account", account]);
        let out = run_capped(&mut cmd, None, cap)?;
        if out.status.success() || out.stderr.is_empty() {
            return Ok(());
        }
        Err(failed(&out))
    } else {
        Err(KeychainCallError::BinaryMissing("<none>".to_string()))
    }
}

/// Never includes stdout — that is where a secret would be.
fn failed(out: &std::process::Output) -> KeychainCallError {
    KeychainCallError::Failed {
        status: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    }
}

fn non_empty(stdout: &[u8]) -> Option<String> {
    let raw = String::from_utf8_lossy(stdout).trim().to_string();
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn decode_go_keyring_base64_unwraps_prefix() {
        let inner = b"{\"token\":{\"access_token\":\"x\",\"token_type\":\"Bearer\",\
                       \"refresh_token\":\"y\",\"expiry\":\"2099-01-01T00:00:00Z\"},\
                       \"auth_method\":\"consumer\"}";
        let b64 = base64::engine::general_purpose::STANDARD.encode(inner);
        let wrapped = format!("go-keyring-base64:{b64}");
        let out = decode_go_keyring_payload(&wrapped).expect("decode");
        assert_eq!(out, inner);
    }

    #[test]
    fn decode_go_keyring_base64_rejects_invalid_alphabet() {
        let wrapped = "go-keyring-base64:!!!not-valid-base64!!!";
        assert_eq!(decode_go_keyring_payload(wrapped), None);
    }

    #[test]
    fn decode_go_keyring_passes_through_when_unprefixed() {
        let raw = "{\"plain\":1}";
        assert_eq!(
            decode_go_keyring_payload(raw),
            Some(raw.as_bytes().to_vec())
        );
    }

    #[test]
    fn decode_hex_round_trips() {
        assert_eq!(decode_hex("deadbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(decode_hex("DEADBEEF"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn decode_hex_rejects_odd_length() {
        assert_eq!(decode_hex("abc"), None);
    }

    #[test]
    fn agent_keychain_files_for_unknown_agent_is_empty() {
        let agent = AgentName::new("totallymadeup").unwrap();
        assert!(agent_keychain_files(&agent).is_empty());
    }

    #[test]
    fn agent_keychain_credentials_for_unknown_agent_is_empty() {
        let agent = AgentName::new("totallymadeup").unwrap();
        assert!(agent_keychain_credentials(&agent).is_empty());
    }

    /// WI 0116 §5: the `security -i` stdin builder is a pure string function,
    /// testable with no keychain. This asserts the exact shape (one
    /// `\n`-terminated `add-generic-password` line), that the `-w` token
    /// carries none of the payload's `"`, `\` or space — for a payload that
    /// deliberately contains all three plus a handful of shell
    /// metacharacters — and round-trips the `-w` token back through
    /// `decode_go_keyring_payload` to prove the read side unwraps exactly
    /// what the write side wrapped.
    #[test]
    fn security_add_generic_password_stdin_carries_no_special_chars_and_round_trips() {
        use crate::data::fs::daemon_env::{
            encode_go_keyring_payload, security_add_generic_password_script,
        };

        let payload: &[u8] = br#"{"TOKEN":"has \" quote, \\ backslash, space, ` ; | & $"}"#;
        let envelope = encode_go_keyring_payload(payload);
        let script = security_add_generic_password_script("awman-squad", "daemon-env", &envelope);

        assert_eq!(
            script.matches('\n').count(),
            1,
            "exactly one line: {script:?}"
        );
        assert!(script.ends_with('\n'), "{script:?}");
        assert_eq!(
            script,
            format!("add-generic-password -U -s awman-squad -a daemon-env -w {envelope}\n")
        );

        let w_value = script
            .trim_end_matches('\n')
            .strip_prefix("add-generic-password -U -s awman-squad -a daemon-env -w ")
            .expect("the -w token must be the final field of the line");
        assert!(!w_value.contains('"'), "{w_value:?}");
        assert!(!w_value.contains('\\'), "{w_value:?}");
        assert!(!w_value.contains(' '), "{w_value:?}");

        let decoded = decode_go_keyring_payload(w_value).expect("the -w token must decode");
        assert_eq!(
            decoded, payload,
            "the read side must unwrap exactly what the write side wrapped"
        );
    }

    #[test]
    fn refreshable_spec_only_for_claude() {
        let claude = AgentName::new("claude").unwrap();
        assert!(refreshable_spec_for(&claude).is_some());
        // Agents without a descriptor keep today's behaviour (env / AgentSecretFile).
        for other in ["antigravity", "codex", "gemini", "totallymadeup"] {
            let agent = AgentName::new(other).unwrap();
            assert!(
                refreshable_spec_for(&agent).is_none(),
                "{other} must have no refresh descriptor"
            );
        }
    }

    /// Remediation of review-security F4 / review-adversarial F8.
    ///
    /// The cap has to cover the stdin write, not only the wait. A child that
    /// never drains its stdin blocks `write_all` as soon as the kernel pipe
    /// buffer fills (64 KiB on Linux), and with the deadline established after
    /// the write the kill-at-expiry path was unreachable: the call blocked
    /// forever, `call_with_cap` detached its thread, and the thread and the
    /// child both leaked for the life of the daemon.
    #[test]
    #[cfg(unix)]
    fn run_capped_times_out_on_a_child_that_never_reads_its_stdin() {
        // Far larger than any pipe buffer, so `write_all` is guaranteed to
        // block on a child that reads nothing.
        let payload = vec![b'x'; 512 * 1024];
        let cap = std::time::Duration::from_millis(300);

        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);

        let started = std::time::Instant::now();
        let outcome = run_capped(&mut cmd, Some(&payload), cap);
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome, Err(KeychainCallError::Timeout)),
            "a child that never drains stdin must time out, not block: {outcome:?}"
        );
        assert!(
            elapsed < cap * 10,
            "the cap must bound the whole call including the write; took {elapsed:?}"
        );
    }

    /// The other half of the same rule: the cap has to cover the child's
    /// *output* too. `security find-generic-password -w` prints the whole
    /// stored payload on stdout, and the daemon's payload is every value it
    /// holds — nothing bounds it to a pipe buffer. A child whose output
    /// exceeds that buffer cannot exit until someone drains it, so a reader
    /// that only starts once `try_wait` reports an exit can never start: the
    /// call spends the whole cap deadlocked and then reports `Timeout`, and a
    /// daemon that reads its own item back degrades and loses persistence.
    #[test]
    #[cfg(unix)]
    fn run_capped_reads_back_a_payload_larger_than_a_pipe_buffer() {
        // 256 KiB of output: four times a Linux pipe buffer, and a plausible
        // size for a stored map holding one PEM key or service-account blob.
        let want = 256 * 1024;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &format!("head -c {want} /dev/zero")]);

        let started = std::time::Instant::now();
        let out = run_capped(&mut cmd, None, std::time::Duration::from_secs(5))
            .expect("a child that writes more than a pipe buffer must not time out");
        assert!(out.status.success());
        assert_eq!(
            out.stdout.len(),
            want,
            "every byte the backend printed must be read back"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the read must not wait out the cap; took {:?}",
            started.elapsed()
        );
    }

    /// The ordinary path still round-trips a payload larger than a pipe buffer
    /// through a child that *does* read it, so the writer-thread change did not
    /// turn a working call into a truncated one.
    #[test]
    #[cfg(unix)]
    fn run_capped_feeds_a_draining_child_a_payload_larger_than_a_pipe_buffer() {
        let payload = vec![b'x'; 256 * 1024];
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "wc -c"]);

        let out = run_capped(&mut cmd, Some(&payload), std::time::Duration::from_secs(5))
            .expect("a draining child must succeed");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            payload.len().to_string(),
            "every byte must reach the child"
        );
    }
}
