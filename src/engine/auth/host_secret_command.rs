//! `HostSecretCommand` — the only host process awman spawns to obtain a
//! workflow-prerequisite credential (WI 0149), and the second sanctioned
//! host-execution exception after [`crate::engine::ready::host_agent::HostAgentPinger`].
//!
//! Hardening, all enforced here rather than by callers:
//!
//! - argv only, never a shell; `argv[0]` is resolved with `which` to an
//!   absolute path, which is shown in every review prompt and is what runs;
//! - known code agents (and every custom agent definition's binary) are
//!   refused by the file name of `argv[0]`, of its resolved path and of the
//!   canonical target, so a symlink `op -> claude` is refused too;
//! - a fresh private 0700 scratch directory outside the repository is the
//!   working directory; the environment is cleared down to an allowlist;
//! - stdin is `/dev/null` unless the caller released the terminal (`Tty`);
//! - the run is time-bounded and the process group is SIGKILLed on timeout,
//!   on cancellation and on drop (the `host_cli::checked_output` pattern);
//! - output is the first non-empty stdout line, validated as a token, and is
//!   returned only as a [`SecretString`]; stderr is capped and scrubbed.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::credential::SecretString;
use crate::data::config::env::host_var;

pub const HOST_SECRET_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);
pub const HOST_SECRET_TIMEOUT_MAX: Duration = Duration::from_secs(300);
pub const HOST_SECRET_MAX_STDOUT_BYTES: usize = 8 * 1024;
pub const HOST_SECRET_MAX_STDERR_SHOWN: usize = 4 * 1024;
/// Known code agents, compared case-insensitively against the file name of argv[0] AND of its canonical target.
pub const REFUSED_AGENT_BINARIES: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "gemini",
    "copilot",
    "cline",
    "antigravity",
    "agy",
    "goose",
    "aider",
];
/// `TERM` is passed only when stdin is the terminal.
pub const HOST_ENV_PASSTHROUGH_EXACT: &[&str] =
    &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR", "TERM"];
pub const HOST_ENV_PASSTHROUGH_PREFIXES: &[&str] = &["XDG_", "OP_", "GH_"];
/// The names under [`HOST_ENV_PASSTHROUGH_PREFIXES`] that are passed. Only
/// Layer 0 may read the process environment and it offers per-name reads, not
/// enumeration, so the prefixes are realised as this fixed list of the
/// variables the XDG base-dir spec, the 1Password CLI and the GitHub CLI
/// document. Every entry starts with one of the prefixes.
pub const HOST_ENV_PASSTHROUGH_KNOWN: &[&str] = &[
    "XDG_CONFIG_HOME",
    "XDG_CONFIG_DIRS",
    "XDG_DATA_HOME",
    "XDG_DATA_DIRS",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "OP_ACCOUNT",
    "OP_SERVICE_ACCOUNT_TOKEN",
    "OP_CONNECT_HOST",
    "OP_CONNECT_TOKEN",
    "OP_CONFIG_DIR",
    "OP_CACHE",
    "OP_BIOMETRIC_UNLOCK_ENABLED",
    "OP_FORMAT",
    "OP_ISO_TIMESTAMPS",
    "GH_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GH_HOST",
    "GH_CONFIG_DIR",
    "GH_PROMPT_DISABLED",
    "GH_NO_UPDATE_NOTIFIER",
];

/// Programs that exist to run another program named in their arguments.
/// Their remaining arguments are checked against the refusal list as well, so
/// `env claude …` or `npx codex` cannot launder an agent past it.
const EXEC_WRAPPERS: &[&str] = &[
    "env", "sudo", "doas", "nohup", "nice", "ionice", "timeout", "time", "xargs", "stdbuf",
    "command", "exec", "npx", "bunx", "pnpx", "pnpm", "yarn", "npm", "node", "bun", "deno", "uvx",
    "pipx", "flatpak", "snap", "nix", "brew",
];

/// Bytes of stderr read back before scrubbing and capping.
const STDERR_READ_LIMIT: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStdin {
    Tty,
    Null,
}

#[derive(Debug, Clone)]
pub struct HostSecretCommandOptions {
    pub argv: Vec<String>,
    /// "gh auth token", "configured token command (op read …)"
    pub label: String,
    /// Clamped to 1s..=[`HOST_SECRET_TIMEOUT_MAX`].
    pub timeout: Duration,
    pub stdin: HostStdin,
    /// Extra refused binaries: every custom agent definition's binary name (Layer 2 supplies them).
    pub refused_binaries: Vec<String>,
    /// false for `gh auth login` (no token expected; stdout ignored).
    pub expect_secret: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum HostSecretCommandError {
    #[error("host command has no program")]
    EmptyArgv,
    #[error("{program} is not on PATH")]
    NotFound { program: String },
    #[error("{program} resolves to {resolved}, a code agent; awman never runs an agent on the host from a token command")]
    RefusedAgentBinary { program: String, resolved: PathBuf },
    #[error("spawn {program}: {reason}")]
    Spawn { program: String, reason: String },
    #[error("{label} timed out after {after:?}")]
    TimedOut { label: String, after: Duration },
    #[error("{label} exited with {code:?}: {stderr}")]
    NonZeroExit {
        label: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("{label} printed nothing on stdout")]
    EmptyOutput { label: String },
    #[error("{label} printed {bytes} bytes; a token is at most 8 KiB")]
    OutputTooLarge { label: String, bytes: usize },
    #[error("{label} output is not a token: {reason}")]
    NotAToken { label: String, reason: String },
}

/// The result of a successful run. No `Serialize`; `Debug` redacts the secret.
pub struct HostSecretOutput {
    pub secret: Option<SecretString>,
    /// Scrubbed of the secret, control characters removed, capped at 4 KiB.
    pub stderr: String,
    /// Non-empty stdout lines after the first (a warning for the caller).
    pub extra_lines: usize,
}

impl std::fmt::Debug for HostSecretOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostSecretOutput")
            .field("secret", &self.secret)
            .field("stderr", &self.stderr)
            .field("extra_lines", &self.extra_lines)
            .finish()
    }
}

/// What a frontend shows before a host command runs. Holds no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostSecretCommandReview {
    pub label: String,
    pub argv: Vec<String>,
    /// `shell_words::join(argv)`, control characters escaped. Display only.
    pub rendered: String,
    /// The absolute path that will actually be executed.
    pub resolved_path: PathBuf,
    pub timeout_secs: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub struct HostSecretCommand {
    argv: Vec<String>,
    label: String,
    resolved: PathBuf,
    timeout: Duration,
    stdin: HostStdin,
    expect_secret: bool,
    executable_hash: String,
}

impl HostSecretCommand {
    pub fn outside_repository(self, root: Option<&Path>) -> Result<Self, HostSecretCommandError> {
        let root = root
            .map(std::fs::canonicalize)
            .transpose()
            .map_err(|error| HostSecretCommandError::Spawn {
                program: self.argv[0].clone(),
                reason: format!("cannot verify workflow repository boundary: {error}"),
            })?;
        if root.is_some_and(|root| self.resolved.starts_with(root)) {
            return Err(HostSecretCommandError::Spawn {
                program: self.argv[0].clone(),
                reason: "host token tools must be installed outside the workflow repository".into(),
            });
        }
        Ok(self)
    }
    /// Resolves argv[0] with `which` (absolute paths must exist and be a regular executable), canonicalizes, applies the refusal list.
    pub fn new(options: HostSecretCommandOptions) -> Result<Self, HostSecretCommandError> {
        let HostSecretCommandOptions {
            argv,
            label,
            timeout,
            stdin,
            refused_binaries,
            expect_secret,
        } = options;
        let program = argv
            .first()
            .filter(|p| !p.trim().is_empty())
            .ok_or(HostSecretCommandError::EmptyArgv)?
            .clone();
        let resolved = resolve_program(&program)?;
        let canonical = resolved
            .canonicalize()
            .map_err(|_| HostSecretCommandError::NotFound {
                program: program.clone(),
            })?;
        if !is_executable_file(&canonical) {
            return Err(HostSecretCommandError::NotFound { program });
        }

        let refused: Vec<String> = REFUSED_AGENT_BINARIES
            .iter()
            .map(|s| s.to_string())
            .chain(
                refused_binaries
                    .iter()
                    .filter_map(|b| program_stem(Path::new(b))),
            )
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let is_refused =
            |path: &Path| program_stem(path).is_some_and(|stem| refused.contains(&stem));
        if is_refused(Path::new(&program)) || is_refused(&resolved) || is_refused(&canonical) {
            return Err(HostSecretCommandError::RefusedAgentBinary { program, resolved });
        }
        let wrapper = |path: &Path| {
            program_stem(path).is_some_and(|stem| {
                EXEC_WRAPPERS.contains(&stem.as_str())
                    || [
                        "sh",
                        "bash",
                        "dash",
                        "zsh",
                        "fish",
                        "ksh",
                        "python",
                        "python3",
                        "perl",
                        "ruby",
                        "lua",
                        "php",
                        "powershell",
                        "pwsh",
                    ]
                    .contains(&stem.as_str())
            })
        };
        let fingerprint = executable_hash(&canonical)?;
        let known_agent = REFUSED_AGENT_BINARIES
            .iter()
            .map(|s| s.to_string())
            .chain(refused_binaries.iter().cloned())
            .any(|name| {
                resolve_program(&name)
                    .ok()
                    .and_then(|p| p.canonicalize().ok())
                    .is_some_and(|p| {
                        p == canonical
                            || executable_hash(&p).ok().as_deref() == Some(fingerprint.as_str())
                    })
            });
        if wrapper(Path::new(&program)) || wrapper(&canonical) || known_agent {
            return Err(HostSecretCommandError::RefusedAgentBinary {
                program,
                resolved: canonical,
            });
        }
        // Agent mounts include the checkout, not trusted user-installed tools.
        // Refuse executable content anywhere inside a Git checkout.
        if canonical
            .ancestors()
            .skip(1)
            .any(|p| p.join(".git").symlink_metadata().is_ok())
        {
            return Err(HostSecretCommandError::Spawn {
                program,
                reason: "token executable must be installed outside every Git checkout".into(),
            });
        }
        let resolved = canonical;
        Ok(Self {
            argv,
            label,
            resolved,
            timeout: timeout.clamp(Duration::from_secs(1), HOST_SECRET_TIMEOUT_MAX),
            stdin,
            expect_secret,
            executable_hash: fingerprint,
        })
    }

    pub fn review(&self) -> HostSecretCommandReview {
        HostSecretCommandReview {
            label: self.label.clone(),
            argv: self.argv.clone(),
            rendered: escape_control(&shell_words::join(&self.argv)),
            resolved_path: self.resolved.clone(),
            timeout_secs: self.timeout.as_secs(),
            sha256: self.sha256(),
        }
    }

    /// Approval hash binds argv, canonical executable path and executable content.
    pub fn sha256(&self) -> String {
        crate::data::fs::hash::sha256_hex(
            serde_json::to_vec(&(&self.argv, &self.resolved, &self.executable_hash))
                .expect("argv serializes"),
        )
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn resolved_path(&self) -> &Path {
        &self.resolved
    }

    /// Blocking. Fresh 0700 TempDir cwd outside the repo; `env_clear()` + passthrough lists; `process_group(0)`;
    /// Bounded stdout/stderr pipes are drained while running; overflow terminates
    /// the group. Error diagnostics are withheld when a secret cannot be scrubbed safely.
    pub fn run(&self) -> Result<HostSecretOutput, HostSecretCommandError> {
        let program = self.argv[0].clone();
        if !cfg!(unix) {
            return Err(HostSecretCommandError::Spawn { program, reason: "host token commands require Unix process-group containment on this platform; use an environment or pasted token".into() });
        }
        let spawn_error = |reason: String| HostSecretCommandError::Spawn {
            program: program.clone(),
            reason,
        };
        let scratch = private_scratch_dir().map_err(|e| spawn_error(e.to_string()))?;
        let work = scratch.path().join("work");
        crate::data::fs::private_files::create_private_dir(&work)
            .map_err(|e| spawn_error(e.to_string()))?;
        if executable_hash(&self.resolved)? != self.executable_hash {
            return Err(spawn_error(
                "token executable changed since review; review the new executable".into(),
            ));
        }
        // Execute an immutable-for-this-run snapshot, not the writable pathname
        // checked above. Its digest is part of the approval identity.
        let executable = scratch.path().join("token-tool");
        let mut source = open_executable(&self.resolved)?;
        let mut snapshot = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&executable)
            .map_err(|e| spawn_error(e.to_string()))?;
        std::io::copy(&mut source, &mut snapshot).map_err(|e| spawn_error(e.to_string()))?;
        drop(snapshot);
        if executable_hash(&executable)? != self.executable_hash {
            return Err(spawn_error("token executable changed while staging".into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500))
                .map_err(|e| spawn_error(e.to_string()))?;
        }
        // An interactive command that expects no token (`gh auth login`)
        // talks to the user directly on the released terminal.
        let interactive = self.stdin == HostStdin::Tty && !self.expect_secret;
        let mut command = Command::new(&executable);
        command
            .args(&self.argv[1..])
            .current_dir(&work)
            .env_clear()
            .envs(minimal_host_env(self.stdin));
        match self.stdin {
            HostStdin::Tty => command.stdin(Stdio::inherit()),
            HostStdin::Null => command.stdin(Stdio::null()),
        };
        if interactive {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        // A concurrent fork elsewhere in this process can inherit the write
        // descriptor used to stage the snapshot, so `execve` reports it busy
        // (`ETXTBSY`) until that fork execs. A refused exec never ran the
        // program, so retrying briefly is safe.
        let mut busy_retries = 0;
        let spawned = loop {
            match command.spawn() {
                Err(e)
                    if e.kind() == std::io::ErrorKind::ExecutableFileBusy && busy_retries < 50 =>
                {
                    busy_retries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => break result,
            }
        };
        let mut child = spawned.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HostSecretCommandError::NotFound {
                    program: program.clone(),
                }
            } else {
                spawn_error(e.to_string())
            }
        })?;
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stdout = child
            .stdout
            .take()
            .map(|pipe| bounded_reader(pipe, HOST_SECRET_MAX_STDOUT_BYTES, overflow.clone()));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| bounded_reader(pipe, STDERR_READ_LIMIT, overflow.clone()));
        let mut guard = RunningChild {
            group: child.id() as i32,
            child: Some(child),
        };
        #[cfg(unix)]
        let _terminal = ForegroundGroup::claim(self.stdin, guard.group)
            .map_err(|e| spawn_error(e.to_string()))?;
        #[cfg(unix)]
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(guard.group),
            nix::sys::signal::Signal::SIGCONT,
        );
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if overflow.load(std::sync::atomic::Ordering::Acquire) {
                guard.kill();
                return Err(HostSecretCommandError::OutputTooLarge {
                    label: self.label.clone(),
                    bytes: HOST_SECRET_MAX_STDOUT_BYTES + 1,
                });
            }
            if crate::engine::host_cli::check_collection_cancel().is_err() {
                guard.kill();
                return Err(spawn_error("cancelled".into()));
            }
            if Instant::now() >= deadline {
                guard.kill();
                return Err(HostSecretCommandError::TimedOut {
                    label: self.label.clone(),
                    after: self.timeout,
                });
            }
            match guard.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(POLL_INTERVAL),
                Err(e) => {
                    guard.kill();
                    return Err(spawn_error(e.to_string()));
                }
            }
        };
        drop(guard);

        let raw_stdout = stdout
            .map(|reader| {
                reader
                    .join()
                    .map_err(|_| spawn_error("stdout reader failed".into()))
            })
            .transpose()?
            .unwrap_or_else(|| SecretString::new(""));
        let raw_stderr = stderr
            .map(|reader| {
                reader
                    .join()
                    .map_err(|_| spawn_error("stderr reader failed".into()))
            })
            .transpose()?
            .unwrap_or_else(|| SecretString::new(""));
        if overflow.load(std::sync::atomic::Ordering::Acquire) {
            return Err(HostSecretCommandError::OutputTooLarge {
                label: self.label.clone(),
                bytes: HOST_SECRET_MAX_STDOUT_BYTES + 1,
            });
        }
        let stdout_len = raw_stdout.expose().len();
        let scrub_values: Vec<&str> = raw_stdout
            .expose()
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let shown_stderr = if status.success() {
            scrub_stderr(raw_stderr.expose(), &scrub_values)
        } else {
            "host command diagnostics withheld".into()
        };
        if !status.success() {
            return Err(HostSecretCommandError::NonZeroExit {
                label: self.label.clone(),
                code: status.code(),
                stderr: shown_stderr,
            });
        }
        if !self.expect_secret {
            return Ok(HostSecretOutput {
                secret: None,
                stderr: shown_stderr,
                extra_lines: 0,
            });
        }
        if stdout_len > HOST_SECRET_MAX_STDOUT_BYTES {
            return Err(HostSecretCommandError::OutputTooLarge {
                label: self.label.clone(),
                bytes: stdout_len,
            });
        }
        let (secret, extra_lines) = parse_token(raw_stdout.expose(), &self.label)?;
        Ok(HostSecretOutput {
            secret: Some(secret),
            stderr: shown_stderr,
            extra_lines,
        })
    }
}

/// The child's environment: the allowlist, read through `host_var` (so the
/// squad daemon passes what its operator's shell would). `TERM` only when
/// stdin is the terminal; unset and non-UTF-8 values are omitted.
pub fn minimal_host_env(stdin: HostStdin) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = HOST_ENV_PASSTHROUGH_EXACT
        .iter()
        .filter(|name| **name != "TERM" || stdin == HostStdin::Tty)
        .chain(HOST_ENV_PASSTHROUGH_KNOWN)
        .filter_map(|name| Some((name.to_string(), host_var(name)?)))
        .collect();
    env.sort();
    env
}

/// Kills the child (its whole group when it has one) unless it was reaped.
struct RunningChild {
    child: Option<Child>,
    group: i32,
}

impl RunningChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        Ok(status)
    }

    fn kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.group),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// `which` for a bare name; an absolute path is taken as given. A relative
/// path (`./op`) or a `PATH` entry that is itself relative would resolve
/// against the working directory, which may be the repository, so both are
/// refused.
fn resolve_program(program: &str) -> Result<PathBuf, HostSecretCommandError> {
    let not_found = || HostSecretCommandError::NotFound {
        program: program.to_string(),
    };
    let path = Path::new(program);
    let resolved = if path.components().count() > 1 || path.is_absolute() {
        path.to_path_buf()
    } else {
        which::which(program).map_err(|_| not_found())?
    };
    if !resolved.is_absolute() {
        return Err(not_found());
    }
    Ok(resolved)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Lower-cased file name without a Windows executable extension.
fn program_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let stem = [".exe", ".cmd", ".bat", ".ps1"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
        .unwrap_or(&name)
        .to_string();
    (!stem.is_empty()).then_some(stem)
}

fn private_scratch_dir() -> std::io::Result<tempfile::TempDir> {
    let scratch = tempfile::Builder::new()
        .prefix("awman-host-secret-")
        .tempdir_in(if cfg!(unix) {
            Path::new("/var/tmp")
        } else {
            Path::new(".")
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(scratch)
}

fn bounded_reader(
    pipe: impl Read + Send + 'static,
    limit: usize,
    overflow: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<SecretString> {
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(limit + 1);
        if pipe
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() > limit
        {
            overflow.store(true, std::sync::atomic::Ordering::Release);
        }
        match String::from_utf8(bytes) {
            Ok(text) => SecretString::new(text),
            Err(error) => {
                let mut bytes = error.into_bytes();
                bytes.fill(0);
                std::hint::black_box(&bytes);
                SecretString::new("\0")
            }
        }
    })
}

fn executable_hash(path: &Path) -> Result<String, HostSecretCommandError> {
    let mut file = open_executable(path)?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut bytes = [0u8; 8192];
    loop {
        let n = file
            .read(&mut bytes)
            .map_err(|e| HostSecretCommandError::Spawn {
                program: path.display().to_string(),
                reason: e.to_string(),
            })?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn open_executable(path: &Path) -> Result<std::fs::File, HostSecretCommandError> {
    let error = |e: std::io::Error| HostSecretCommandError::Spawn {
        program: path.display().to_string(),
        reason: e.to_string(),
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(error)?;
    if !file.metadata().map_err(error)?.is_file() {
        return Err(error(std::io::Error::other(
            "token executable must be a regular file",
        )));
    }
    Ok(file)
}

#[cfg(unix)]
struct ForegroundGroup {
    previous: Option<nix::unistd::Pid>,
    mask: nix::sys::signal::SigSet,
}
#[cfg(unix)]
impl ForegroundGroup {
    fn claim(stdin: HostStdin, group: i32) -> std::io::Result<Self> {
        use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
        let mut old = SigSet::empty();
        let mut blocked = SigSet::empty();
        blocked.add(Signal::SIGTTOU);
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&blocked), Some(&mut old))?;
        let terminal = std::io::stdin();
        let previous = if stdin == HostStdin::Tty {
            nix::unistd::tcgetpgrp(&terminal).ok()
        } else {
            None
        };
        let guard = Self {
            previous,
            mask: old,
        };
        if previous.is_some() {
            nix::unistd::tcsetpgrp(&terminal, nix::unistd::Pid::from_raw(group))?;
        }
        Ok(guard)
    }
}
#[cfg(unix)]
impl Drop for ForegroundGroup {
    fn drop(&mut self) {
        if let Some(group) = self.previous {
            let _ = nix::unistd::tcsetpgrp(std::io::stdin(), group);
        }
        let _ = nix::sys::signal::pthread_sigmask(
            nix::sys::signal::SigmaskHow::SIG_SETMASK,
            Some(&self.mask),
            None,
        );
    }
}

/// First non-empty line, trimmed, validated; plus the count of further
/// non-empty lines.
fn parse_token(stdout: &str, label: &str) -> Result<(SecretString, usize), HostSecretCommandError> {
    let mut lines = stdout.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(first) = lines.next() else {
        return Err(HostSecretCommandError::EmptyOutput {
            label: label.to_string(),
        });
    };
    let reason = if first.chars().any(char::is_whitespace) {
        Some("it contains whitespace")
    } else if first.chars().any(char::is_control) {
        Some("it contains control characters")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(HostSecretCommandError::NotAToken {
            label: label.to_string(),
            reason: reason.to_string(),
        });
    }
    Ok((SecretString::new(first), lines.count()))
}

/// Remove `values`, drop control characters other than newline/tab, cap at
/// [`HOST_SECRET_MAX_STDERR_SHOWN`] on a character boundary. Scrubbing runs
/// before the cap so a token can never be cut into a surviving fragment.
fn scrub_stderr(stderr: &str, values: &[&str]) -> String {
    let mut text = stderr.to_string();
    let mut values = values.to_vec();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for value in values {
        text = text.replace(value, super::redaction::RedactionSet::MARK);
    }
    let text: String = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    let text = text.trim();
    if text.len() <= HOST_SECRET_MAX_STDERR_SHOWN {
        return text.to_string();
    }
    let mut end = HOST_SECRET_MAX_STDERR_SHOWN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn escape_control(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn options(argv: Vec<String>) -> HostSecretCommandOptions {
        HostSecretCommandOptions {
            argv,
            label: "test command".into(),
            timeout: Duration::from_secs(10),
            stdin: HostStdin::Null,
            refused_binaries: vec![],
            expect_secret: true,
        }
    }

    fn run(path: &Path, args: &[&str]) -> Result<HostSecretOutput, HostSecretCommandError> {
        let mut argv = vec![path.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|a| a.to_string()));
        HostSecretCommand::new(options(argv))?.run()
    }

    #[test]
    fn returns_first_line_as_secret_and_counts_extra_lines() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(
            dir.path(),
            "tok",
            "printf '\\n  tok_123456  \\nsecond\\n\\nthird\\n'",
        );
        let out = run(&s, &[]).unwrap();
        assert_eq!(out.secret.as_ref().unwrap().expose(), "tok_123456");
        assert_eq!(out.extra_lines, 2);
        assert!(!format!("{out:?}").contains("tok_123456"));
    }

    #[test]
    fn argv_metacharacters_reach_the_program_literally() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(
            dir.path(),
            "echoarg",
            "printf '%s\\n' \"$1\" | tr ' ;$' '_:%'",
        );
        let out = run(&s, &["$(id) ; `x`"]).unwrap();
        assert_eq!(out.secret.unwrap().expose(), "%(id)_:_`x`");
    }

    #[test]
    fn validates_output() {
        let dir = tempfile::tempdir().unwrap();
        let empty = script(dir.path(), "empty", "printf '\\n\\n'");
        assert!(matches!(
            run(&empty, &[]),
            Err(HostSecretCommandError::EmptyOutput { .. })
        ));
        let spaced = script(dir.path(), "spaced", "echo 'two words'");
        assert!(matches!(
            run(&spaced, &[]),
            Err(HostSecretCommandError::NotAToken { .. })
        ));
        let ctrl = script(dir.path(), "ctrl", "printf 'ab\\033cd\\n'");
        assert!(matches!(
            run(&ctrl, &[]),
            Err(HostSecretCommandError::NotAToken { .. })
        ));
        let big = script(dir.path(), "big", "head -c 9000 /dev/zero | tr '\\0' a");
        assert!(matches!(
            run(&big, &[]),
            Err(HostSecretCommandError::OutputTooLarge { .. })
        ));
    }

    #[test]
    fn non_zero_exit_scrubs_the_token_from_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(
            dir.path(),
            "fail",
            "echo tok_abcdef; echo 'bad tok_abcdef' >&2; exit 3",
        );
        let Err(HostSecretCommandError::NonZeroExit { code, stderr, .. }) = run(&s, &[]) else {
            panic!("expected NonZeroExit");
        };
        assert_eq!(code, Some(3));
        assert_eq!(stderr, "host command diagnostics withheld");
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let s = script(
            dir.path(),
            "slow",
            &format!("(sleep 2; touch '{}') & sleep 30", marker.display()),
        );
        let mut opts = options(vec![s.to_string_lossy().into_owned()]);
        opts.timeout = Duration::from_millis(1);
        let started = Instant::now();
        let result = HostSecretCommand::new(opts).unwrap().run();
        assert!(matches!(
            result,
            Err(HostSecretCommandError::TimedOut { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "background child outlived the group kill");
    }

    #[test]
    fn refuses_agents_by_name_symlink_target_wrapper_and_custom_list() {
        let dir = tempfile::tempdir().unwrap();
        let agent = script(dir.path(), "claude", "echo nope");
        assert!(matches!(
            HostSecretCommand::new(options(vec![agent.to_string_lossy().into()])),
            Err(HostSecretCommandError::RefusedAgentBinary { .. })
        ));
        let link = dir.path().join("op");
        std::os::unix::fs::symlink(&agent, &link).unwrap();
        assert!(matches!(
            HostSecretCommand::new(options(vec![link.to_string_lossy().into()])),
            Err(HostSecretCommandError::RefusedAgentBinary { .. })
        ));
        let env = script(dir.path(), "env", "echo nope");
        assert!(matches!(
            HostSecretCommand::new(options(vec![env.to_string_lossy().into(), "Codex".into()])),
            Err(HostSecretCommandError::RefusedAgentBinary { .. })
        ));
        let crush = script(dir.path(), "crush", "echo nope");
        let mut opts = options(vec![crush.to_string_lossy().into()]);
        opts.refused_binaries = vec!["/usr/local/bin/Crush".into()];
        assert!(matches!(
            HostSecretCommand::new(opts),
            Err(HostSecretCommandError::RefusedAgentBinary { .. })
        ));
    }

    #[test]
    fn relative_and_missing_programs_are_not_found() {
        assert!(matches!(
            HostSecretCommand::new(options(vec!["./op".into()])),
            Err(HostSecretCommandError::NotFound { .. })
        ));
        assert!(matches!(
            HostSecretCommand::new(options(vec!["/nonexistent/awman-op".into()])),
            Err(HostSecretCommandError::NotFound { .. })
        ));
        assert!(matches!(
            HostSecretCommand::new(options(vec![])),
            Err(HostSecretCommandError::EmptyArgv)
        ));
    }

    #[test]
    fn runs_in_a_private_empty_dir_with_a_minimal_env() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(
            dir.path(),
            "probe",
            "[ -z \"$(ls -A .)\" ] || exit 1\n\
             [ \"$(stat -c %a . 2>/dev/null || stat -f %Lp .)\" = 700 ] || exit 2\n\
             env | grep -q '^TERM=' && exit 4\n\
             read -r line && exit 5\n\
             echo ok_token",
        );
        let out = run(&s, &[]);
        assert_eq!(out.unwrap().secret.unwrap().expose(), "ok_token");
        let names: Vec<String> = minimal_host_env(HostStdin::Null)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert!(names
            .iter()
            .all(|k| HOST_ENV_PASSTHROUGH_EXACT.contains(&k.as_str())
                || HOST_ENV_PASSTHROUGH_PREFIXES
                    .iter()
                    .any(|p| k.starts_with(p))));
        assert!(!names.contains(&"TERM".to_string()));
        assert!(HOST_ENV_PASSTHROUGH_KNOWN
            .iter()
            .all(|k| HOST_ENV_PASSTHROUGH_PREFIXES
                .iter()
                .any(|p| k.starts_with(p))));
    }

    #[test]
    fn child_environment_contains_only_allowlisted_names() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(
            dir.path(),
            "envnames",
            "env | cut -d= -f1 | grep -v -E '^(PWD|OLDPWD|SHLVL|_)$' | sort | tr '\\n' ','",
        );
        let out = run(&s, &[]).unwrap();
        let printed = out.secret.unwrap();
        for name in printed.expose().split(',').filter(|n| !n.is_empty()) {
            assert!(
                HOST_ENV_PASSTHROUGH_EXACT.contains(&name)
                    || HOST_ENV_PASSTHROUGH_KNOWN.contains(&name),
                "unexpected variable {name} reached the child"
            );
            assert_ne!(
                name, "TERM",
                "TERM is only passed when stdin is the terminal"
            );
        }
        for secret_looking in [
            "GITHUB_TOKEN",
            "AWMAN_API_KEY",
            "SSH_AUTH_SOCK",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!printed.expose().split(',').any(|n| n == secret_looking));
        }
    }

    #[test]
    fn stdin_is_dev_null_for_headless_runs() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(
            dir.path(),
            "stdin",
            "if read -r line; then exit 9; fi; echo eof_token",
        );
        let out = run(&s, &[]).unwrap();
        assert_eq!(out.secret.unwrap().expose(), "eof_token");
    }

    #[test]
    fn output_of_exactly_the_limit_is_accepted_and_over_it_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let at_limit = script(dir.path(), "limit", "head -c 8192 /dev/zero | tr '\\0' a");
        let out = run(&at_limit, &[]).unwrap();
        assert_eq!(out.secret.unwrap().expose().len(), 8192);
        let over = script(dir.path(), "over", "head -c 8193 /dev/zero | tr '\\0' a");
        assert!(matches!(
            run(&over, &[]),
            Err(HostSecretCommandError::OutputTooLarge { bytes: 8193, .. })
        ));
    }

    #[test]
    fn whitespace_only_and_non_utf8_output_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let blank = script(dir.path(), "blank", "printf '   \\n\\t\\n'");
        assert!(matches!(
            run(&blank, &[]),
            Err(HostSecretCommandError::EmptyOutput { .. })
        ));
        let binary = script(dir.path(), "bin", "printf '\\377\\376\\n'");
        assert!(run(&binary, &[]).is_err());
    }

    #[test]
    fn stderr_is_capped_and_scrubbed_on_success_too() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(
            dir.path(),
            "noisy",
            "echo tok_secret_77; echo \"warn tok_secret_77 $(head -c 6000 /dev/zero | tr '\\0' x)\" >&2",
        );
        let error = run(&s, &[]).unwrap_err();
        assert!(matches!(
            error,
            HostSecretCommandError::OutputTooLarge { .. }
        ));
        assert!(!error.to_string().contains("tok_secret_77"));
    }

    #[test]
    fn scrub_stderr_removes_longest_values_first_and_control_characters() {
        assert_eq!(
            scrub_stderr(
                "a tok_long_value b tok_long c",
                &["tok_long", "tok_long_value"]
            ),
            "a *** b *** c"
        );
        assert_eq!(scrub_stderr("x\u{1b}[31mred\u{0}", &[]), "x[31mred");
        let long = "é".repeat(HOST_SECRET_MAX_STDERR_SHOWN);
        let capped = scrub_stderr(&long, &[]);
        assert!(capped.ends_with('…'));
        assert!(capped.len() <= HOST_SECRET_MAX_STDERR_SHOWN + 4);
    }

    #[test]
    fn timeout_is_clamped_to_one_second_minimum_and_five_minutes_maximum() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(dir.path(), "ok", "echo tok_ok_value");
        let mut opts = options(vec![s.to_string_lossy().into_owned()]);
        opts.timeout = Duration::from_secs(100_000);
        assert_eq!(
            HostSecretCommand::new(opts).unwrap().review().timeout_secs,
            300
        );
        let mut opts = options(vec![s.to_string_lossy().into_owned()]);
        opts.timeout = Duration::from_millis(1);
        assert_eq!(
            HostSecretCommand::new(opts).unwrap().review().timeout_secs,
            1
        );
    }

    #[test]
    fn every_documented_agent_binary_is_refused_regardless_of_case() {
        let dir = tempfile::tempdir().unwrap();
        for agent in [
            "claude", "codex", "opencode", "gemini", "copilot", "cline", "Claude", "CODEX",
        ] {
            let sub = tempfile::tempdir_in(dir.path()).unwrap();
            let path = script(sub.path(), agent, "echo nope");
            assert!(
                matches!(
                    HostSecretCommand::new(options(vec![path.to_string_lossy().into()])),
                    Err(HostSecretCommandError::RefusedAgentBinary { .. })
                ),
                "{agent} must be refused"
            );
        }
        for wrapper in ["npx", "env", "sudo", "timeout"] {
            let sub = tempfile::tempdir_in(dir.path()).unwrap();
            let path = script(sub.path(), wrapper, "echo nope");
            assert!(
                matches!(
                    HostSecretCommand::new(options(vec![
                        path.to_string_lossy().into(),
                        "claude".into()
                    ])),
                    Err(HostSecretCommandError::RefusedAgentBinary { .. })
                ),
                "{wrapper} claude"
            );
        }
    }

    #[test]
    fn review_renders_escaped_argv_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let s = script(dir.path(), "op", "echo x");
        let argv = vec![
            s.to_string_lossy().into_owned(),
            "read".into(),
            "a\nb".into(),
        ];
        let cmd = HostSecretCommand::new(options(argv.clone())).unwrap();
        let review = cmd.review();
        assert!(review.rendered.contains("\\n"));
        assert!(!review.rendered.contains('\n'));
        assert_eq!(review.resolved_path, s.canonicalize().unwrap());
        assert_eq!(review.sha256, cmd.sha256());
        assert_eq!(review.timeout_secs, 10);
    }
}

#[cfg(all(test, unix))]
mod review_regression_tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn options(path: &Path) -> HostSecretCommandOptions {
        HostSecretCommandOptions {
            argv: vec![path.display().to_string()],
            label: "fixture".into(),
            timeout: Duration::from_secs(1),
            stdin: HostStdin::Null,
            refused_binaries: vec![],
            expect_secret: true,
        }
    }
    #[test]
    fn interpreter_code_and_renamed_custom_agent_are_refused() {
        let mut opts = options(Path::new("/bin/sh"));
        opts.argv.extend(["-c".into(), "echo bad".into()]);
        assert!(HostSecretCommand::new(opts).is_err());
        let dir = tempfile::tempdir().unwrap();
        let agent = script(dir.path(), "my-agent", "echo marker");
        let copy = dir.path().join("op");
        std::fs::copy(&agent, &copy).unwrap();
        let mut opts = options(&copy);
        opts.refused_binaries.push(agent.display().to_string());
        assert!(HostSecretCommand::new(opts).is_err());
    }
    #[test]
    fn symlink_swap_uses_canonical_tool_and_file_replacement_requires_review() {
        let dir = tempfile::tempdir().unwrap();
        let safe = script(dir.path(), "safe", "echo approved_token");
        let evil = script(dir.path(), "claude", "echo unauthorized_token");
        let link = dir.path().join("op");
        symlink(&safe, &link).unwrap();
        let cmd = HostSecretCommand::new(options(&link)).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&evil, &link).unwrap();
        assert_eq!(
            cmd.run().unwrap().secret.unwrap().expose(),
            "approved_token"
        );
        std::fs::write(safe, "#!/bin/sh\necho changed_token\n").unwrap();
        assert!(cmd.run().is_err());
        assert_ne!(
            cmd.sha256(),
            HostSecretCommand::new(options(&link))
                .err()
                .map(|_| "refused".to_string())
                .unwrap()
        );
    }
    #[test]
    fn overflow_failure_never_returns_late_secret_or_unbounded_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            dir.path(),
            "op",
            "head -c 20000 /dev/zero | tr '\\000' x; echo late_secret >&2; exit 1",
        );
        let error = HostSecretCommand::new(options(&path))
            .unwrap()
            .run()
            .unwrap_err()
            .to_string();
        assert!(!error.contains("late_secret"));
    }
    #[test]
    fn descendants_die_after_success_failure_and_tty_timeout() {
        for (body, stdin) in [
            ("echo accepted_token", HostStdin::Null),
            ("exit 1", HostStdin::Null),
            ("sleep 5", HostStdin::Tty),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("survived");
            let path = script(
                dir.path(),
                "op",
                &format!("(sleep 2; touch '{}') &\n{body}", marker.display()),
            );
            let mut opts = options(&path);
            opts.stdin = stdin;
            let _ = HostSecretCommand::new(opts).unwrap().run();
            std::thread::sleep(Duration::from_millis(2200));
            assert!(!marker.exists(), "descendant survived {stdin:?}");
        }
    }
    #[test]
    fn repository_executables_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let path = script(dir.path(), "op", "echo not_trusted");
        assert!(HostSecretCommand::new(options(&path)).is_err());
    }

    #[test]
    fn directory_workspace_alias_cannot_hide_a_repository_executable() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&workspace, &alias).unwrap();
        let path = script(&workspace, "op", "echo not_trusted");
        assert!(HostSecretCommand::new(options(&path))
            .unwrap()
            .outside_repository(Some(&alias))
            .is_err());
    }
}

#[cfg(all(test, unix))]
mod cancellation_review_tests {
    use super::*;
    #[test]
    fn cancellation_kills_a_running_descendant_group() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fixture-tool");
        let marker = dir.path().join("survived");
        let started = dir.path().join("started");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n(sleep 1; touch '{}') &\necho ready > '{}'\nsleep 5\n",
                marker.display(),
                started.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = HostSecretCommand::new(HostSecretCommandOptions {
            argv: vec![script.display().to_string()],
            label: "fixture".into(),
            timeout: Duration::from_secs(5),
            stdin: HostStdin::Null,
            refused_binaries: vec![],
            expect_secret: true,
        })
        .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let signal = cancel.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..100 {
                if started.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            signal.cancel();
        });
        assert!(crate::engine::host_cli::with_collection_cancel(cancel, || command.run()).is_err());
        worker.join().unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!marker.exists());
    }
}
