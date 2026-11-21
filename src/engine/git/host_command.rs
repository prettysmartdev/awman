//! Host Git must not run repository-supplied hooks, monitors or content drivers.
use crate::data::config::env::EnvSnapshot;
use std::path::Path;
use std::process::Command;

pub(super) fn command(snapshot: &EnvSnapshot, cwd: &Path) -> Command {
    let mut command = discovery_command(snapshot, cwd);
    if cwd.join(".git").exists() {
        // The session/lifecycle owns the mount root. Writable core.worktree
        // configuration must never redirect host staging or checkout.
        command
            .arg("-c")
            .arg(format!("core.worktree={}", cwd.display()));
    }
    command
}

pub(super) fn discovery_command(snapshot: &EnvSnapshot, cwd: &Path) -> Command {
    // Legacy local callers have their own error handling. The guarded path
    // used for publication must propagate probe failures before any work.
    discovery_command_guarded(
        snapshot,
        cwd,
        &super::transport::TransportGuard::new(tokio_util::sync::CancellationToken::new()),
    )
    .unwrap_or_else(|_| {
        let mut refused = crate::engine::host_cli::command(snapshot, "git");
        refused.arg("--awman-config-probe-failed");
        refused
    })
}

pub(super) fn command_guarded(
    snapshot: &EnvSnapshot,
    cwd: &Path,
    guard: &super::transport::TransportGuard,
) -> Result<Command, crate::engine::error::EngineError> {
    let mut command = discovery_command_guarded(snapshot, cwd, guard)?;
    if cwd.join(".git").exists() {
        command
            .arg("-c")
            .arg(format!("core.worktree={}", cwd.display()));
    }
    Ok(command)
}

fn discovery_command_guarded(
    snapshot: &EnvSnapshot,
    cwd: &Path,
    guard: &super::transport::TransportGuard,
) -> Result<Command, crate::engine::error::EngineError> {
    let mut command = crate::engine::host_cli::command(snapshot, "git");
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env("GIT_NO_LAZY_FETCH", "1")
        // Host git runs behind frontends that cannot answer a credential
        // prompt (a TUI tab, the daemon). Fail instead of blocking on one.
        .env("GIT_TERMINAL_PROMPT", "0");
    for setting in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "commit.gpgSign=false",
        "tag.gpgSign=false",
        "push.gpgSign=false",
        "maintenance.auto=false",
        "gc.auto=0",
        "diff.external=",
        "protocol.ext.allow=never",
        "submodule.recurse=false",
        "core.alternateRefsCommand=",
    ] {
        command.args(["-c", setting]);
    }
    // Attribute names are arbitrary. Disable every configured executable
    // content driver, including those selected by .git/info/attributes.
    // `git config` only reads settings; it does not execute their values.
    let mut probe = crate::engine::host_cli::command(snapshot, "git");
    probe
        .args(["config", "--null", "--name-only", "--get-regexp", "^(filter\\..*\\.(clean|smudge|process)|diff\\..*\\.(command|textconv)|merge\\..*\\.driver)$"])
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null()).current_dir(cwd);
    let output = guard.output(&mut probe)?;
    if matches!(output.status.code(), Some(0 | 1)) {
        for key in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|key| !key.is_empty())
        {
            let key = String::from_utf8_lossy(key);
            let value = if key.starts_with("merge.") {
                "false"
            } else {
                ""
            };
            command.arg("-c").arg(format!("{key}={value}"));
            if let Some(name) = key
                .strip_prefix("filter.")
                .and_then(|s| s.rsplit_once('.').map(|p| p.0))
            {
                command
                    .arg("-c")
                    .arg(format!("filter.{name}.required=false"));
            }
        }
    } else {
        return Err(crate::engine::error::EngineError::Git(
            "cannot read content driver configuration".into(),
        ));
    }
    Ok(command)
}

/// Repository-scoped settings that select an executable, a credential
/// source or a network path for transport. Agents can write the repository's
/// `.git/config`, so a repo-local (or worktree-local) value for any of these
/// is refused before host git talks to a remote. Global settings are the
/// user's own and stay in force.
const UNTRUSTED_TRANSPORT_KEYS: &str = "^(core\\.(sshcommand|gitproxy|askpass)|credential\\..*helper|remote\\..*\\.(vcs|uploadpack|receivepack|proxy)|http\\..*(extraheader|proxy|sslverify|sslcainfo|sslcapath|sslcert|sslkey|cookiefile|curloptresolve))$";

/// Upper bound for one host network git operation (`ls-remote`, `fetch`).
pub(super) const NETWORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// A [`command`] for an operation that contacts `remote`. On top of the
/// local hardening it refuses repository-scoped transport executables and
/// credential sources, pins the remote's upload/receive-pack programs, and
/// makes SSH headless unless the user chose an SSH command through
/// their environment or global configuration.
pub(super) fn transport_command(
    snapshot: &EnvSnapshot,
    cwd: &Path,
    remote: &str,
) -> Result<Command, crate::engine::error::EngineError> {
    transport_command_guarded(
        snapshot,
        cwd,
        remote,
        &super::transport::TransportGuard::new(tokio_util::sync::CancellationToken::new()),
    )
}
pub(super) fn transport_command_guarded(
    snapshot: &EnvSnapshot,
    cwd: &Path,
    remote: &str,
    guard: &super::transport::TransportGuard,
) -> Result<Command, crate::engine::error::EngineError> {
    let mut command = command_guarded(snapshot, cwd, guard)?;
    let mut user_ssh_command = false;
    for (scope, key) in config_keys_with_scope(snapshot, cwd, UNTRUSTED_TRANSPORT_KEYS, guard)? {
        if !matches!(scope.as_str(), "global" | "system" | "command") {
            return Err(crate::engine::error::EngineError::Git(format!(
                "refusing to contact remote '{remote}': repository configuration sets `{key}` ({scope} scope), which could run a program or redirect credentials on the host. Remove it from the repository's Git config (or move it to your global config) and retry."
            )));
        }
        user_ssh_command |= key == "core.sshcommand";
    }
    let env_ssh = snapshot
        .get_os(crate::data::config::env::GIT_SSH_COMMAND)
        .is_some()
        || snapshot.get_os(crate::data::config::env::GIT_SSH).is_some();
    if !env_ssh && !user_ssh_command {
        // ssh opens /dev/tty for host-key and passphrase prompts, which
        // GIT_TERMINAL_PROMPT does not cover.
        command.args(["-c", "core.sshCommand=ssh -o BatchMode=yes"]);
    }
    command
        .arg("-c")
        .arg(format!("remote.{remote}.uploadpack=git-upload-pack"))
        .arg("-c")
        .arg(format!("remote.{remote}.receivepack=git-receive-pack"))
        .args(["-c", "core.askPass="]);
    Ok(command)
}

/// `(scope, key)` for every configured key matching `pattern`. A failure to
/// read configuration is an error, never "nothing configured".
fn config_keys_with_scope(
    snapshot: &EnvSnapshot,
    cwd: &Path,
    pattern: &str,
    guard: &super::transport::TransportGuard,
) -> Result<Vec<(String, String)>, crate::engine::error::EngineError> {
    let mut probe = crate::engine::host_cli::command(snapshot, "git");
    probe
        .args([
            "config",
            "--show-scope",
            "--null",
            "--name-only",
            "--get-regexp",
            pattern,
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .current_dir(cwd);
    let output = guard.output(&mut probe)?;
    // Exit 1 means no key matched.
    match output.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(crate::engine::error::EngineError::Git(
                "cannot read repository configuration".into(),
            ))
        }
    }
    let fields: Vec<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect();
    Ok(fields
        .chunks(2)
        .filter(|pair| pair.len() == 2)
        .map(|pair| (pair[0].clone(), pair[1].to_ascii_lowercase()))
        .collect())
}

/// Run `command` with a null stdin, capturing output through files (so a
/// descendant holding a pipe cannot hang the read), and kill its process
/// group if it runs longer than `timeout`.
pub(super) fn bounded_output(
    command: &mut Command,
    timeout: std::time::Duration,
    what: &str,
) -> Result<std::process::Output, crate::engine::error::EngineError> {
    use std::io::{Read, Seek};
    let io = |e: std::io::Error| crate::engine::error::EngineError::Git(format!("{what}: {e}"));
    let mut stdout = tempfile::tempfile().map_err(io)?;
    let mut stderr = tempfile::tempfile().map_err(io)?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(stdout.try_clone().map_err(io)?)
        .stderr(stderr.try_clone().map_err(io)?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(io)?;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(io)? {
            break status;
        }
        if start.elapsed() >= timeout {
            #[cfg(unix)]
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
            return Err(crate::engine::error::EngineError::Git(format!(
                "{what} timed out after {} seconds",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout
        .rewind()
        .and_then(|_| stdout.read_to_end(&mut out))
        .map_err(io)?;
    stderr
        .rewind()
        .and_then(|_| stderr.read_to_end(&mut err))
        .map_err(io)?;
    Ok(std::process::Output {
        status,
        stdout: out,
        stderr: err,
    })
}
