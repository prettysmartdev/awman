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
use crate::engine::agent::definition::CredentialSource;
use crate::engine::agent::first_party::first_party;
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
    agent_keychain_credentials_in(&crate::engine::host_cli::process_env(), agent)
}

pub fn agent_keychain_credentials_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &AgentName,
) -> Vec<(String, String)> {
    match credential_source(agent) {
        CredentialSource::ClaudeKeychainOauth => claude_keychain_credentials_in(snapshot),
        CredentialSource::AntigravityKeychainFile | CredentialSource::None => Vec::new(),
    }
}

/// This agent's credential scheme, from the first-party table
/// (`first_party.rs`). Credential schemes are first-party-only facts: any
/// other name (a custom agent, or an unknown one) has no host credential
/// awman passes through — which is exactly what the `_ =>` arms these three
/// functions used to carry said. No file I/O.
fn credential_source(agent: &AgentName) -> CredentialSource {
    first_party(agent.as_str())
        .map(|d| d.credential_source)
        .unwrap_or(CredentialSource::None)
}

/// File-form credentials for the agent. Empty when the platform has no
/// keychain integration, the entry is missing, or the payload fails to decode.
pub fn agent_keychain_files(agent: &AgentName) -> Vec<AgentSecretFile> {
    agent_keychain_files_in(&crate::engine::host_cli::process_env(), agent)
}

pub fn agent_keychain_files_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &AgentName,
) -> Vec<AgentSecretFile> {
    match credential_source(agent) {
        CredentialSource::AntigravityKeychainFile => antigravity_keychain_files_in(snapshot),
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
fn claude_keychain_credentials_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
) -> Vec<(String, String)> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let source = HostCredentialSource::MacosKeychain {
        service: CLAUDE_KEYCHAIN_SERVICE,
    };
    match credential::read_claude_credential_in(snapshot, &source) {
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
fn antigravity_keychain_files_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
) -> Vec<AgentSecretFile> {
    let Some(raw) = read_antigravity_secret_in(snapshot) else {
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

fn read_antigravity_secret_in(snapshot: &crate::data::config::env::EnvSnapshot) -> Option<String> {
    if cfg!(target_os = "macos") {
        run_macos_keychain_lookup_in(snapshot, "gemini", Some("antigravity"))
    } else if cfg!(target_os = "linux") {
        run_linux_secret_lookup_in(snapshot, "gemini", "antigravity")
    } else {
        None
    }
}

// ── Keychain backend selection ──────────────────────────────────────────────

/// A process-local stand-in for the OS keychain: generic-password items keyed
/// by `(service, account)`, holding exactly the string the OS backend would.
/// Starts empty, so every lookup is a clean "no such item" until something in
/// the same process stores one.
mod memory {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, MutexGuard};

    static ITEMS: Mutex<BTreeMap<(String, String), String>> = Mutex::new(BTreeMap::new());

    fn items() -> MutexGuard<'static, BTreeMap<(String, String), String>> {
        // A poisoned map is still a whole map: every write replaces one entry.
        ITEMS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `account: None` matches any account, as `security find-generic-password`
    /// without `-a` does.
    pub(super) fn lookup(service: &str, account: Option<&str>) -> Option<String> {
        items()
            .iter()
            .find(|((s, a), _)| s == service && account.is_none_or(|want| want == a))
            .map(|(_, value)| value.clone())
    }

    pub(super) fn store(service: &str, account: &str, value: &str) {
        items().insert(
            (service.to_string(), account.to_string()),
            value.to_string(),
        );
    }

    pub(super) fn clear(service: &str, account: &str) {
        items().remove(&(service.to_string(), account.to_string()));
    }
}

// ── Shared OS keychain shims ────────────────────────────────────────────────

pub(crate) fn run_macos_keychain_lookup_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    account: Option<&str>,
) -> Option<String> {
    if cfg!(test) || snapshot.test_isolation() {
        return memory::lookup(service, account);
    }
    let mut cmd = crate::engine::host_cli::command(snapshot, "security");
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

fn run_linux_secret_lookup_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    account: &str,
) -> Option<String> {
    if cfg!(test) || snapshot.test_isolation() {
        return memory::lookup(service, Some(account));
    }
    let out = crate::engine::host_cli::command(snapshot, "secret-tool")
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
pub enum KeychainCallError {
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
///
/// Public so the bounded-call contract is tested against real child processes
/// (component tier); production callers stay inside this module.
pub fn run_capped(
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
    keychain_lookup_in(
        &crate::engine::host_cli::process_env(),
        service,
        account,
        cap,
    )
}

pub(crate) fn keychain_lookup_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    account: &str,
    cap: std::time::Duration,
) -> Result<Option<String>, KeychainCallError> {
    if cfg!(test) || snapshot.test_isolation() {
        return Ok(memory::lookup(service, Some(account)));
    }
    if cfg!(target_os = "macos") {
        let mut cmd = crate::engine::host_cli::command(snapshot, "security");
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
        let mut cmd = crate::engine::host_cli::command(snapshot, "secret-tool");
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
    keychain_store_in(
        &crate::engine::host_cli::process_env(),
        service,
        account,
        envelope,
        cap,
    )
}

pub(crate) fn keychain_store_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    account: &str,
    envelope: &str,
    cap: std::time::Duration,
) -> Result<(), KeychainCallError> {
    if cfg!(test) || snapshot.test_isolation() {
        memory::store(service, account, envelope);
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let script = crate::data::fs::daemon_env::security_add_generic_password_script(
            service, account, envelope,
        );
        let mut cmd = crate::engine::host_cli::command(snapshot, "security");
        cmd.arg("-i");
        let out = run_capped(&mut cmd, Some(script.as_bytes()), cap)?;
        if !out.status.success() {
            return Err(failed(&out));
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        let mut cmd = crate::engine::host_cli::command(snapshot, "secret-tool");
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
    keychain_clear_in(
        &crate::engine::host_cli::process_env(),
        service,
        account,
        cap,
    )
}

pub(crate) fn keychain_clear_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    account: &str,
    cap: std::time::Duration,
) -> Result<(), KeychainCallError> {
    if cfg!(test) || snapshot.test_isolation() {
        memory::clear(service, account);
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let mut cmd = crate::engine::host_cli::command(snapshot, "security");
        cmd.args(["delete-generic-password", "-s", service, "-a", account]);
        let out = run_capped(&mut cmd, None, cap)?;
        if out.status.success() || out.status.code() == Some(MACOS_ITEM_NOT_FOUND) {
            return Ok(());
        }
        Err(failed(&out))
    } else if cfg!(target_os = "linux") {
        let mut cmd = crate::engine::host_cli::command(snapshot, "secret-tool");
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
