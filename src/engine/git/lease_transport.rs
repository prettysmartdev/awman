//! The shared host Git transport for pulled leases.
//!
//! Every network Git operation of a lease (clone, setup fetch, merge-ref and
//! push-back `ls-remote`, push) is built by one function,
//! [`lease_transport_command`]; `RemotePushGit::with_lease_transport` routes
//! its push-back `ls-remote` and push through it too. It applies one
//! configuration:
//!
//! - no system or global Git configuration (`GIT_CONFIG_NOSYSTEM=1`,
//!   `GIT_CONFIG_GLOBAL=/dev/null`), so neither the host's credential helpers
//!   nor its URL rewrites can apply to a server-chosen repository;
//! - every inherited credential helper reset (`credential.helper=`), then a
//!   one-shot helper that answers only for the lease repository's host and
//!   path (`credential.useHttpPath=true`, so Git sends the path it asks for).
//!
//! Credentials never appear in a URL, in argv, in logs, in `.git/config` or
//! in any persisted config. They live in an owner-only (0600) file inside an
//! owner-only (0700) temporary directory outside the clone, whose path is
//! passed to the helper through [`LEASE_CREDENTIAL_FILE_ENV`]. The helper's
//! argv carries only a random nonce that must match the file. The directory
//! is removed when the helper is dropped.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::auth_classify::GitFailureClass;
use super::{host_command, GitCommand, GitEngine, GitFrontend};
use crate::data::config::env::EnvSnapshot;
use crate::data::secret::SecretString;
use crate::engine::error::EngineError;

/// Environment variable naming the helper's credential file. Only the hidden
/// `__git-credential-helper` command reads it.
pub const LEASE_CREDENTIAL_FILE_ENV: &str = crate::data::config::env::AWMAN_LEASE_CREDENTIAL_FILE;
/// The hidden catalogue command fronting [`answer_credential_request`].
pub const CREDENTIAL_HELPER_SUBCOMMAND: &str = "__git-credential-helper";
/// Username presented with a bearer token (accepted by GitHub and GitLab).
pub const BEARER_USERNAME: &str = "x-access-token";
/// Upper bound for one lease network Git operation.
pub const LEASE_NETWORK_TIMEOUT: Duration = Duration::from_secs(300);

type Result<T> = std::result::Result<T, EngineError>;

fn cfg_err(message: impl Into<String>) -> EngineError {
    EngineError::Config(message.into())
}

/// Repository credentials of a lease, as the work server issued them.
pub struct LeaseCredentials {
    kind: awman_wsp::CredentialKind,
    username: Option<String>,
    secret: Option<SecretString>,
    expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for LeaseCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseCredentials")
            .field("kind", &self.kind)
            .field("username", &self.username.as_ref().map(|_| "[redacted]"))
            .field("secret", &self.secret.as_ref().map(|_| "[redacted]"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

fn clean_value(value: &str, what: &str) -> Result<()> {
    if value.is_empty() || value.contains(['\n', '\r', '\0']) {
        return Err(cfg_err(format!(
            "lease repository {what} is empty or contains a line break"
        )));
    }
    Ok(())
}

impl LeaseCredentials {
    /// No credentials (`kind: none`, or none sent).
    pub fn none() -> Self {
        Self {
            kind: awman_wsp::CredentialKind::None,
            username: None,
            secret: None,
            expires_at: None,
        }
    }

    /// Validate the WSP credentials. `bearer` and `basic` need a token;
    /// `basic` needs a username. Anything malformed fails closed.
    pub fn from_wsp(creds: Option<&awman_wsp::Credentials>) -> Result<Self> {
        let Some(creds) = creds else {
            return Ok(Self::none());
        };
        use awman_wsp::CredentialKind as K;
        let token = creds.token.as_deref();
        let (username, secret) = match creds.kind {
            K::None => (None, None),
            K::Bearer => {
                let token =
                    token.ok_or_else(|| cfg_err("bearer lease credentials have no token"))?;
                clean_value(token, "token")?;
                (
                    Some(BEARER_USERNAME.to_string()),
                    Some(SecretString::new(token)),
                )
            }
            K::Basic => {
                let token =
                    token.ok_or_else(|| cfg_err("basic lease credentials have no token"))?;
                let user = creds
                    .username
                    .as_deref()
                    .ok_or_else(|| cfg_err("basic lease credentials have no username"))?;
                clean_value(token, "token")?;
                clean_value(user, "username")?;
                (Some(user.to_string()), Some(SecretString::new(token)))
            }
        };
        Ok(Self {
            kind: creds.kind.clone(),
            username,
            secret,
            expires_at: creds.expires_at,
        })
    }

    pub fn kind(&self) -> &awman_wsp::CredentialKind {
        &self.kind
    }

    pub fn has_secret(&self) -> bool {
        self.secret.is_some()
    }

    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    /// Whether the credentials expire before `now + margin` (refresh first).
    pub fn expires_within(&self, now: DateTime<Utc>, margin: Duration) -> bool {
        let margin = chrono::Duration::from_std(margin).unwrap_or(chrono::Duration::MAX);
        self.expires_at.is_some_and(|at| at <= now + margin)
    }
}

/// The program Git runs as the credential helper. Production uses the awman
/// binary's hidden [`CREDENTIAL_HELPER_SUBCOMMAND`]; the caller resolves the
/// executable path (Layer 1 does not read the process environment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperProgram {
    program: PathBuf,
    args: Vec<String>,
}

impl HelperProgram {
    pub fn awman(executable: PathBuf) -> Self {
        Self {
            program: executable,
            args: vec![CREDENTIAL_HELPER_SUBCOMMAND.to_string()],
        }
    }

    /// Any program speaking Git's credential protocol, given the nonce and
    /// the action as its last two arguments.
    pub fn custom(program: PathBuf, args: Vec<String>) -> Self {
        Self { program, args }
    }

    fn helper_value(&self, nonce: &str) -> String {
        let mut value = format!("!{}", shell_quote(&self.program.to_string_lossy()));
        for arg in &self.args {
            value.push(' ');
            value.push_str(&shell_quote(arg));
        }
        value.push(' ');
        value.push_str(nonce);
        value
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Which remote a helper answers for: protocol, host (with port) and the
/// repository path, as Git sends it with `credential.useHttpPath=true`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CredentialScope {
    protocol: String,
    host: String,
    path: String,
}

/// A repository path the way Git's credential protocol carries it:
/// percent-decoded, without leading or trailing `/`.
fn credential_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                decoded.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded)
        .trim_matches('/')
        .to_string()
}

/// Which repository URLs a lease may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeaseUrlPolicy {
    /// Allow `file://` and local paths (tests with a local bare repository).
    /// Never set for a server-supplied lease.
    pub allow_local: bool,
}

/// Check a lease repository URL: HTTPS, HTTP to a loopback host, or SSH;
/// never credentials in the URL. Returns the HTTP scope, if any.
fn check_url(url: &str, policy: LeaseUrlPolicy) -> Result<Option<CredentialScope>> {
    if policy.allow_local
        && (url.starts_with('/') || url.starts_with('.') || url.starts_with("file://"))
    {
        return Ok(None);
    }
    awman_wsp::repo_url::validate(url).map_err(cfg_err)?;
    if let Ok(parsed) = reqwest::Url::parse(url) {
        if matches!(parsed.scheme(), "https" | "http") {
            let host = parsed
                .host_str()
                .ok_or_else(|| cfg_err("lease repository URL has no host"))?;
            let authority = parsed
                .port()
                .map_or_else(|| host.to_string(), |port| format!("{host}:{port}"));
            return Ok(Some(CredentialScope {
                protocol: parsed.scheme().into(),
                host: authority,
                path: credential_path(parsed.path()),
            }));
        }
    }
    Ok(None)
}

/// The installed one-shot credential helper of one lease.
pub struct LeaseCredentialHelper {
    dir: tempfile::TempDir,
    file: PathBuf,
    nonce: String,
    program: HelperProgram,
    scope: CredentialScope,
}

impl std::fmt::Debug for LeaseCredentialHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseCredentialHelper")
            .field("file", &self.file)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl LeaseCredentialHelper {
    /// Write `creds` for `repo_url` into a new owner-only directory under
    /// `parent` (which is created owner-only). `parent` must be outside every
    /// directory mounted into a container.
    pub fn install(
        parent: &Path,
        repo_url: &str,
        creds: &LeaseCredentials,
        program: HelperProgram,
        policy: LeaseUrlPolicy,
    ) -> Result<Self> {
        let scope = check_url(repo_url, policy)?.ok_or_else(|| {
            cfg_err("lease credentials are only supported for HTTPS repository URLs")
        })?;
        if !creds.has_secret() {
            return Err(cfg_err("lease credentials carry no secret"));
        }
        crate::data::fs::private_files::create_private_dir(parent)?;
        let dir = tempfile::Builder::new()
            .prefix("awman-lease-cred-")
            .tempdir_in(parent)
            .map_err(|e| EngineError::io(parent, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(|e| EngineError::io(dir.path(), e))?;
        }
        let helper = Self {
            file: dir.path().join("credentials"),
            dir,
            nonce: uuid::Uuid::new_v4().simple().to_string(),
            program,
            scope,
        };
        helper.write(creds)?;
        Ok(helper)
    }

    /// Replace the credentials (a fresh `POST /v1/leases/{id}/credentials`).
    pub fn rotate(&self, creds: &LeaseCredentials) -> Result<()> {
        if !creds.has_secret() {
            return Err(cfg_err("lease credentials carry no secret"));
        }
        self.write(creds)
    }

    fn write(&self, creds: &LeaseCredentials) -> Result<()> {
        let (Some(user), Some(secret)) = (&creds.username, &creds.secret) else {
            return Err(cfg_err("lease credentials carry no secret"));
        };
        let body = format!(
            "nonce={}\nprotocol={}\nhost={}\npath={}\nusername={}\npassword={}\n",
            self.nonce,
            self.scope.protocol,
            self.scope.host,
            self.scope.path,
            user,
            secret.expose()
        );
        let tmp = self
            .dir
            .path()
            .join(format!(".credentials.{}", uuid::Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = crate::data::fs::private_files::create_private_file_new(&tmp)?;
            file.write_all(body.as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|e| EngineError::io(&tmp, e))?;
            std::fs::rename(&tmp, &self.file).map_err(|e| EngineError::io(&self.file, e))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// `-c` arguments installing this helper after resetting every inherited
    /// one. `credential.useHttpPath` makes Git send the repository path, so
    /// the helper can refuse every other repository on the same host.
    pub fn git_config_args(&self) -> Vec<String> {
        vec![
            "-c".into(),
            "credential.useHttpPath=true".into(),
            "-c".into(),
            "credential.helper=".into(),
            "-c".into(),
            format!(
                "credential.helper={}",
                self.program.helper_value(&self.nonce)
            ),
        ]
    }

    /// Environment for the Git child (the file path, never its contents).
    pub fn env(&self) -> Vec<(String, String)> {
        vec![(
            LEASE_CREDENTIAL_FILE_ENV.to_string(),
            self.file.to_string_lossy().into_owned(),
        )]
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
}

/// Answer one Git credential-helper invocation (`get | store | erase`).
/// `get` prints the username and password from `file` only when `nonce`
/// matches the file and the request's protocol, host and path match the
/// lease repository; anything else, including a request without a path,
/// prints nothing (Git then fails without a prompt). `store` and `erase` are no-ops: nothing is ever cached.
pub fn answer_credential_request(
    action: &str,
    nonce: &str,
    file: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<()> {
    let mut request = BTreeMap::new();
    let mut line = String::new();
    loop {
        line.clear();
        if input
            .read_line(&mut line)
            .map_err(|e| EngineError::io(file, e))?
            == 0
        {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            request.insert(k.to_string(), v.to_string());
        }
    }
    if action != "get" {
        return Ok(());
    }
    let mut contents = String::new();
    std::io::Read::read_to_string(
        &mut crate::data::fs::private_files::open_private_file(file)?,
        &mut contents,
    )
    .map_err(|e| EngineError::io(file, e))?;
    let contents = SecretString::new(contents);
    let stored: BTreeMap<&str, &str> = contents
        .expose()
        .lines()
        .filter_map(|l| l.split_once('='))
        .collect();
    let matches = |key: &str| {
        request.get(key).map(|v| v.to_ascii_lowercase())
            == stored.get(key).map(|v| v.to_ascii_lowercase())
    };
    // Protocol and host compare case-insensitively; the path exactly.
    let path_matches = match (request.get("path"), stored.get("path")) {
        (Some(asked), Some(approved)) => credential_path(asked) == *approved,
        _ => false,
    };
    if stored.get("nonce") != Some(&nonce)
        || !matches("protocol")
        || !matches("host")
        || !path_matches
    {
        return Ok(());
    }
    if let (Some(user), Some(password)) = (stored.get("username"), stored.get("password")) {
        write!(output, "username={user}\npassword={password}\n")
            .map_err(|e| EngineError::io(file, e))?;
    }
    Ok(())
}

/// The lease transport for every network operation of one lease.
#[derive(Debug, Clone, Default)]
pub struct LeaseTransport {
    helper: Option<Arc<LeaseCredentialHelper>>,
    policy: LeaseUrlPolicy,
}

impl LeaseTransport {
    /// No credentials: the remote must allow anonymous access.
    pub fn anonymous(policy: LeaseUrlPolicy) -> Self {
        Self {
            helper: None,
            policy,
        }
    }

    pub fn with_helper(helper: Arc<LeaseCredentialHelper>, policy: LeaseUrlPolicy) -> Self {
        Self {
            helper: Some(helper),
            policy,
        }
    }

    pub fn helper(&self) -> Option<&Arc<LeaseCredentialHelper>> {
        self.helper.as_ref()
    }

    /// Refuse a URL this transport may not contact.
    pub fn check_url(&self, url: &str) -> Result<()> {
        check_url(url, self.policy).map(|_| ())
    }

    /// Defense in depth for arbitrary remote output, including Basic encodings.
    pub fn redact(&self, text: &str) -> String {
        use base64::Engine as _;
        let Some(helper) = &self.helper else {
            return text.to_owned();
        };
        let Ok(contents) = std::fs::read_to_string(helper.file()) else {
            return "Git diagnostic unavailable".into();
        };
        let values: BTreeMap<_, _> = contents
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let mut safe = text.to_owned();
        if let Some(password) = values.get("password").filter(|p| !p.is_empty()) {
            let basic = format!("{}:{password}", values.get("username").unwrap_or(&""));
            safe = safe.replace(
                &base64::engine::general_purpose::STANDARD.encode(basic),
                "[redacted]",
            );
            safe = safe.replace(
                &base64::engine::general_purpose::STANDARD.encode(password),
                "[redacted]",
            );
            safe = safe.replace(password, "[redacted]");
        }
        safe
    }

    /// Apply the isolation and helper to `command` (shared by every lease
    /// operation, including `RemotePushGit`'s transport calls).
    pub fn apply(&self, command: &mut Command) {
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .args(["-c", "credential.interactive=false"]);
        if !self.policy.allow_local {
            command.args(["-c", "protocol.file.allow=never"]);
        }
        match &self.helper {
            Some(helper) => {
                command.args(helper.git_config_args()).envs(helper.env());
            }
            None => {
                command.args(["-c", "credential.helper="]);
            }
        }
    }
}

/// A host Git command for a lease network operation against `remote`:
/// the hardened transport command, no system/global config, and the
/// lease's helper. Subcommand arguments follow.
pub fn lease_transport_command(
    snapshot: &EnvSnapshot,
    cwd: &Path,
    remote: &str,
    transport: &LeaseTransport,
) -> Result<Command> {
    let mut command = host_command::transport_command(&snapshot.for_lease_git(), cwd, remote)?;
    transport.apply(&mut command);
    Ok(command)
}

/// Run `command` (display `args`) with a null stdin and file-captured
/// output; kill its process group on `cancel` or after `timeout`. A
/// non-zero exit is classified into `GitAuth`/`GitTransport`/`GitRejected`.
fn run_lease(
    mut command: Command,
    args: &[&str],
    operation: &str,
    sink: &mut dyn GitFrontend,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<std::process::Output> {
    use std::io::{Read, Seek};
    let display = GitCommand::new(args);
    sink.command_started(&display);
    let io = |e: std::io::Error| EngineError::Git(format!("invoke `{}`: {e}", display.display()));
    let mut stdout = tempfile::tempfile().map_err(io)?;
    let mut stderr = tempfile::tempfile().map_err(io)?;
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(stdout.try_clone().map_err(io)?)
        .stderr(stderr.try_clone().map_err(io)?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if cancel.is_cancelled() {
        return Err(cfg_err("lease Git cancelled"));
    }
    let trace = super::auth_classify::ResponseTrace::attach(&mut command)?;
    let mut child = command.spawn().map_err(io)?;
    let started = std::time::Instant::now();
    let mut timed_out;
    let status = loop {
        let cancelled = cancel.is_cancelled();
        timed_out = started.elapsed() >= timeout;
        if cancelled || timed_out {
            #[cfg(unix)]
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let status = child.wait().map_err(io)?;
            if cancelled {
                return Err(EngineError::Git(format!(
                    "`{}` cancelled",
                    display.display()
                )));
            }
            break status;
        }
        match child.try_wait().map_err(io)? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(10)),
        }
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
    let output = std::process::Output {
        status,
        stdout: out,
        stderr: err,
    };
    // Never expose arbitrary transport stderr to logs: a remote can echo
    // credentials. Preserve it only for classification, emit stable outcomes.
    if !output.status.success() {
        sink.write_message(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Warning,
            text: format!(
                "lease Git {operation} failed ({:?})",
                trace.classify(&String::from_utf8_lossy(&output.stderr), timed_out)
            ),
        });
    }
    if timed_out || !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if timed_out {
            format!("timed out after {} seconds", timeout.as_secs())
        } else {
            "lease Git remote operation failed".into()
        };
        let class = trace.classify(&stderr, timed_out);
        let message = if class == GitFailureClass::Transport {
            "connection failed before a remote response (DNS, connect or reset)".into()
        } else {
            message
        };
        return Err(class.into_error(operation, message));
    }
    Ok(output)
}

impl GitEngine {
    /// `git clone [-b branch] <url> <dest>` through the lease transport.
    pub fn clone_with_lease_transport(
        &self,
        url: &str,
        branch: Option<&str>,
        dest: &Path,
        transport: &LeaseTransport,
        sink: &mut dyn GitFrontend,
        cancel: &CancellationToken,
    ) -> Result<()> {
        transport.check_url(url)?;
        let parent = dest
            .parent()
            .ok_or_else(|| EngineError::Git("clone destination has no parent".into()))?;
        std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        let dest = dest
            .to_str()
            .ok_or_else(|| EngineError::Git("clone destination is not UTF-8".into()))?;
        let mut args = vec!["clone", "--no-recurse-submodules"];
        if let Some(branch) = branch {
            args.extend(["-b", branch]);
        }
        args.extend(["--", url, dest]);
        let command = lease_transport_command(self.env(), parent, "origin", transport)?;
        run_lease(command, &args, "clone", sink, cancel, LEASE_NETWORK_TIMEOUT)?;
        Ok(())
    }

    /// `git fetch origin <refspecs…>` in `repo` through the lease transport.
    pub fn fetch_with_lease_transport(
        &self,
        repo: &Path,
        refspecs: &[String],
        transport: &LeaseTransport,
        sink: &mut dyn GitFrontend,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut args = vec!["fetch", "--no-recurse-submodules", "origin"];
        args.extend(refspecs.iter().map(String::as_str));
        let command = lease_transport_command(self.env(), repo, "origin", transport)?;
        run_lease(command, &args, "fetch", sink, cancel, LEASE_NETWORK_TIMEOUT)?;
        Ok(())
    }

    /// `git ls-remote --heads --tags <url>` through the lease transport:
    /// `ref → sha`.
    pub fn ls_remote_with_lease_transport(
        &self,
        url: &str,
        transport: &LeaseTransport,
        sink: &mut dyn GitFrontend,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<String, String>> {
        transport.check_url(url)?;
        let scratch = tempfile::tempdir().map_err(|e| EngineError::Git(e.to_string()))?;
        let command = lease_transport_command(self.env(), scratch.path(), "origin", transport)?;
        let args = ["ls-remote", "--heads", "--tags", "--", url];
        let output = run_lease(
            command,
            &args,
            "ls-remote",
            sink,
            cancel,
            LEASE_NETWORK_TIMEOUT,
        )?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(sha, r)| (r.to_string(), sha.to_string()))
            .collect())
    }
}
