//! Socket-backed one-shot credential helpers for push-session Git (WI 0145).
//!
//! A push session's caller-supplied credential ([`GitAuthSpec`]) exists only
//! in daemon memory. Two listeners hand it to Git without it ever touching
//! argv, the environment, a file or `.git/config`:
//!
//! - [`CredentialSocketServer`] serves **host** Git. One server per daemon
//!   listens on an owner-only Unix socket inside a 0700 directory. Each use
//!   of a session transport takes an [`OperationTicket`]: a random 128-bit
//!   nonce registered with the credential and its repository scope. The
//!   ticket is a [`CredentialHelper`]: Git runs
//!   `awman __git-credential-helper <nonce> get`, which reads the socket
//!   path from [`GIT_CREDENTIAL_SOCKET_ENV`] and relays the request. The
//!   server answers only a same-uid peer, a live nonce (constant-time
//!   compare) and a request for exactly the ticket's protocol, host and
//!   repository path; anything else is closed without output, so Git fails
//!   without a prompt (`git_auth_failed`). Dropping the ticket revokes it.
//! - [`CacheSocketListener`] serves **container** Git (teardown/setup
//!   `push_branch` and `pull_branch`). It speaks Git's own
//!   `credential-cache` socket protocol, so the container needs only `git`
//!   (`credential.helper=cache --socket <path>`). It lives for one command,
//!   answers `get` only for the session's protocol and host (and the
//!   repository path when Git sends one) and ignores `store`, `erase` and
//!   `exit`.
//!
//! Neither listener logs a request or a reply.
//!
//! A container receiving the cache socket is trusted with the full reusable
//! credential. Host/path matching constrains ordinary Git requests; it cannot
//! confine a malicious socket consumer. Use upstream repository-scoped,
//! short-lived credentials for untrusted workflows.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::lease_transport::{read_credential_request, CredentialScope, HelperProgram};
use super::transport::CredentialHelper;
use crate::data::git_transport::GitAuthSpec;
use crate::engine::error::EngineError;

/// Environment variable naming the host helper's socket (a path, never a secret).
pub const GIT_CREDENTIAL_SOCKET_ENV: &str = crate::data::config::env::AWMAN_GIT_CREDENTIAL_SOCKET;
/// Where a command's container listener directory is mounted.
pub const CONTAINER_CREDENTIAL_DIR: &str = "/awman/git-credential";
/// The container listener's socket inside [`CONTAINER_CREDENTIAL_DIR`].
pub const CONTAINER_CREDENTIAL_SOCKET: &str = "/awman/git-credential/sock";

/// Upper bound of one request on either socket.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// A peer that does not finish its request in this time is dropped.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

type Result<T> = std::result::Result<T, EngineError>;

fn cfg_err(message: impl Into<String>) -> EngineError {
    EngineError::Config(message.into())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn answer(credential: &GitAuthSpec) -> String {
    format!(
        "username={}\npassword={}\n",
        credential.username(),
        credential.secret().expose()
    )
}

/// Remove `credential` (raw, and the Basic/base64 encodings an HTTP trace
/// would carry) from `text`.
pub fn redact_credential(credential: &GitAuthSpec, text: &str) -> String {
    use base64::Engine as _;
    let secret = credential.secret().expose();
    let b64 = |value: &str| base64::engine::general_purpose::STANDARD.encode(value);
    text.replace(
        &b64(&format!("{}:{secret}", credential.username())),
        "[redacted]",
    )
    .replace(&b64(secret), "[redacted]")
    .replace(secret, "[redacted]")
}

struct Ticket {
    credential: Arc<GitAuthSpec>,
    scope: CredentialScope,
}

type Tickets = Arc<Mutex<HashMap<String, Ticket>>>;

struct Serving {
    thread: std::thread::JoinHandle<()>,
    active: Arc<AtomicBool>,
}

/// The daemon's host-Git credential socket. See the module documentation.
pub struct CredentialSocketServer {
    _dir: tempfile::TempDir,
    path: PathBuf,
    program: HelperProgram,
    tickets: Tickets,
    stop: Arc<AtomicBool>,
    thread: Option<Serving>,
}

impl std::fmt::Debug for CredentialSocketServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSocketServer")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl CredentialSocketServer {
    /// Bind `<private_dir>/git-cred-<random>/s` (directory 0700, socket 0600)
    /// and serve it on a background thread until dropped. `private_dir` is
    /// created owner-only; keep it short (a Unix socket path is limited to
    /// about 100 bytes).
    pub fn start(private_dir: &Path, program: HelperProgram) -> Result<Arc<Self>> {
        let dir = private_socket_dir(private_dir, "git-cred-", 0o700)?;
        let path = dir.path().join("s");
        let tickets: Tickets = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = imp::serve(
            &path,
            0o600,
            {
                let tickets = Arc::clone(&tickets);
                move |request| answer_ticket(&tickets, request)
            },
            true,
            Arc::clone(&stop),
        )?;
        Ok(Arc::new(Self {
            _dir: dir,
            path,
            program,
            tickets,
            stop,
            thread: Some(thread),
        }))
    }

    /// Register `credential` for one transport use against `scope`. The
    /// nonce answers until the returned ticket is dropped.
    pub fn ticket(
        self: &Arc<Self>,
        credential: Arc<GitAuthSpec>,
        scope: CredentialScope,
    ) -> OperationTicket {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        self.tickets
            .lock()
            .expect("credential tickets lock")
            .insert(
                nonce.clone(),
                Ticket {
                    credential: Arc::clone(&credential),
                    scope,
                },
            );
        OperationTicket {
            server: Arc::clone(self),
            nonce,
            credential,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Tickets currently answering (diagnostics and tests).
    pub fn live_tickets(&self) -> usize {
        self.tickets.lock().expect("credential tickets lock").len()
    }
}

impl Drop for CredentialSocketServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.thread.join();
        }
    }
}

/// The private host-socket protocol: `nonce=`, `action=`, then Git's
/// request lines. Answers `get` for a live nonce whose scope matches.
fn answer_ticket(tickets: &Tickets, request: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(request).ok()?;
    let mut lines = text.lines();
    let nonce = lines.next()?.strip_prefix("nonce=")?;
    let action = lines.next()?.strip_prefix("action=")?;
    if action != "get" {
        return None;
    }
    let rest = lines.collect::<Vec<_>>().join("\n");
    let request = read_credential_request(&mut rest.as_bytes()).ok()?;
    let tickets = tickets.lock().ok()?;
    // Compare against every live nonce so the time taken does not reveal
    // which one (if any) matched.
    let mut found = None;
    for (live, ticket) in tickets.iter() {
        if constant_time_eq(live.as_bytes(), nonce.as_bytes()) {
            found = Some(ticket);
        }
    }
    let ticket = found?;
    ticket
        .scope
        .answers(&request, true)
        .then(|| answer(&ticket.credential))
}

/// One registered use of a session credential; also the transport's helper.
pub struct OperationTicket {
    server: Arc<CredentialSocketServer>,
    nonce: String,
    credential: Arc<GitAuthSpec>,
}

impl std::fmt::Debug for OperationTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationTicket")
            .field("socket", &self.server.path)
            .finish_non_exhaustive()
    }
}

impl Drop for OperationTicket {
    fn drop(&mut self) {
        if let Ok(mut tickets) = self.server.tickets.lock() {
            tickets.remove(&self.nonce);
        }
    }
}

impl CredentialHelper for OperationTicket {
    /// Inherited helpers are already reset by the transport; this installs
    /// only the awman helper with the nonce (no secret).
    fn git_config_args(&self) -> Vec<String> {
        vec![
            "-c".into(),
            "credential.useHttpPath=true".into(),
            "-c".into(),
            format!(
                "credential.helper={}",
                self.server.program.helper_value(&self.nonce)
            ),
        ]
    }
    fn env(&self) -> Vec<(String, String)> {
        vec![(
            GIT_CREDENTIAL_SOCKET_ENV.to_string(),
            self.server.path.to_string_lossy().into_owned(),
        )]
    }
    fn redact(&self, text: &str) -> String {
        redact_credential(&self.credential, text)
    }
}

/// The client half, run by `awman __git-credential-helper <nonce> <action>`
/// when [`GIT_CREDENTIAL_SOCKET_ENV`] is set: relay Git's request to the
/// daemon and copy the reply. `store` and `erase` are no-ops.
pub fn request_via_socket(
    socket: &Path,
    nonce: &str,
    action: &str,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<()> {
    let request = read_credential_request(input).map_err(|e| EngineError::io(socket, e))?;
    if action != "get" {
        return Ok(());
    }
    let mut body = format!("nonce={nonce}\naction=get\n");
    for (key, value) in &request {
        body.push_str(&format!("{key}={value}\n"));
    }
    let reply = imp::exchange(socket, body.as_bytes())?;
    output
        .write_all(&reply)
        .map_err(|e| EngineError::io(socket, e))
}

/// A per-command listener speaking Git's `credential-cache` socket protocol
/// for container Git. See the module documentation.
pub struct CacheSocketListener {
    dir: tempfile::TempDir,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<Serving>,
    scope: CredentialScope,
    credential: Arc<GitAuthSpec>,
}

impl std::fmt::Debug for CacheSocketListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheSocketListener")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl CacheSocketListener {
    /// Bind `<private_dir>/git-cred-cmd-<random>/sock`. The directory is the
    /// mount source; it is 0711 and the socket 0666 so a container user with
    /// another uid can connect. Host users are kept out by `private_dir`,
    /// which is created owner-only (0700).
    pub fn start(
        private_dir: &Path,
        credential: Arc<GitAuthSpec>,
        scope: CredentialScope,
    ) -> Result<Self> {
        let dir = private_socket_dir(private_dir, "git-cred-cmd-", 0o711)?;
        let socket = dir.path().join("sock");
        let stop = Arc::new(AtomicBool::new(false));
        let answer_scope = scope.clone();
        let answer_credential = credential.clone();
        let thread = imp::serve(
            &socket,
            0o666,
            move |request| answer_cache(&answer_credential, &answer_scope, request),
            false,
            Arc::clone(&stop),
        )?;
        Ok(Self {
            dir,
            socket,
            stop,
            thread: Some(thread),
            scope,
            credential,
        })
    }

    /// The host directory to mount at [`CONTAINER_CREDENTIAL_DIR`].
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
    pub fn matches_url(&self, url: &str) -> bool {
        self.scope.matches_url(url)
    }
    pub fn has_active_request(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|task| task.active.load(Ordering::SeqCst))
    }
    pub fn register_redaction(&self, set: &crate::engine::auth::redaction::RedactionSet) {
        use base64::Engine as _;
        set.add(self.credential.secret().clone());
        set.add(crate::data::secret::SecretString::new(
            base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{}",
                self.credential.username(),
                self.credential.secret().expose()
            )),
        ));
    }
}

impl Drop for CacheSocketListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.thread.join();
        }
    }
}

/// `credential-cache` request: `action=…`, `timeout=…`, then the
/// credential lines; the client half-closes and reads the reply to EOF.
fn answer_cache(credential: &GitAuthSpec, scope: &CredentialScope, raw: &[u8]) -> Option<String> {
    let request = read_credential_request(&mut &raw[..]).ok()?;
    if request.get("action").map(String::as_str) != Some("get") {
        return None;
    }
    scope.answers(&request, true).then(|| answer(credential))
}

fn private_socket_dir(parent: &Path, prefix: &str, mode: u32) -> Result<tempfile::TempDir> {
    crate::data::fs::private_files::create_private_dir(parent)?;
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .rand_bytes(8)
        .tempdir_in(parent)
        .map_err(|e| EngineError::io(parent, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode))
            .map_err(|e| EngineError::io(dir.path(), e))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(dir)
}

/// Read a request of at most [`MAX_REQUEST_BYTES`] (the peer half-closes).
fn read_request(stream: &mut dyn Read) -> Option<Vec<u8>> {
    let mut request = Vec::new();
    stream
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut request)
        .ok()?;
    (request.len() as u64 <= MAX_REQUEST_BYTES).then_some(request)
}

/// Whether this platform can serve session credentials over a socket.
pub fn supported() -> bool {
    cfg!(unix)
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Bind `path` with `mode` and answer every connection with `reply` on a
    /// background thread until `stop` is set. With `same_uid`, a peer of
    /// another uid is closed without output.
    pub(super) fn serve(
        path: &Path,
        mode: u32,
        reply: impl Fn(&[u8]) -> Option<String> + Send + 'static,
        same_uid: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<Serving> {
        use std::os::unix::fs::PermissionsExt;
        if path.as_os_str().len() > 100 {
            return Err(cfg_err(format!(
                "credential socket path is too long: {}",
                path.display()
            )));
        }
        let listener = UnixListener::bind(path).map_err(|e| EngineError::io(path, e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| EngineError::io(path, e))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| EngineError::io(path, e))?;
        let active = Arc::new(AtomicBool::new(false));
        let serving = active.clone();
        let thread = std::thread::Builder::new()
            .name("awman-git-credential".into())
            .spawn(move || loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                };
                if same_uid && !peer_is_us(&stream) {
                    continue;
                }
                serving.store(true, Ordering::SeqCst);
                let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(20)));
                let end = std::time::Instant::now() + IO_TIMEOUT;
                let mut request = Vec::new();
                let mut complete = false;
                while !stop.load(Ordering::SeqCst)
                    && std::time::Instant::now() < end
                    && request.len() as u64 <= MAX_REQUEST_BYTES
                {
                    let mut buf = [0u8; 4096];
                    match stream.read(&mut buf) {
                        Ok(0) => {
                            complete = true;
                            break;
                        }
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(_) => break,
                    }
                }
                if complete
                    && request.len() as u64 <= MAX_REQUEST_BYTES
                    && !stop.load(Ordering::SeqCst)
                {
                    if let Some(answer) = reply(&request) {
                        if !stop.load(Ordering::SeqCst) {
                            let _ = stream.write_all(answer.as_bytes());
                        }
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
                serving.store(false, Ordering::SeqCst);
            })
            .map_err(|e| EngineError::io(path, e))?;
        Ok(Serving { thread, active })
    }

    pub(super) fn exchange(socket: &Path, body: &[u8]) -> Result<Vec<u8>> {
        let io = |e| EngineError::io(socket, e);
        let mut stream = UnixStream::connect(socket).map_err(io)?;
        stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(io)?;
        stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;
        stream.write_all(body).map_err(io)?;
        stream.shutdown(std::net::Shutdown::Write).map_err(io)?;
        read_request(&mut stream).ok_or_else(|| cfg_err("credential reply too large"))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_is_us(stream: &UnixStream) -> bool {
        use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
        getsockopt(stream, PeerCredentials)
            .is_ok_and(|cred| cred.uid() == nix::unistd::geteuid().as_raw())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn peer_is_us(stream: &UnixStream) -> bool {
        nix::unistd::getpeereid(stream).is_ok_and(|(uid, _)| uid == nix::unistd::geteuid())
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;
    pub(super) fn serve(
        _path: &Path,
        _mode: u32,
        _reply: impl Fn(&[u8]) -> Option<String> + Send + 'static,
        _same_uid: bool,
        _stop: Arc<AtomicBool>,
    ) -> Result<Serving> {
        Err(cfg_err(
            "session Git credentials are not supported on this platform",
        ))
    }
    pub(super) fn exchange(_socket: &Path, _body: &[u8]) -> Result<Vec<u8>> {
        Err(cfg_err(
            "session Git credentials are not supported on this platform",
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::data::git_transport::GitAuthKind;
    use crate::data::secret::SecretString;

    fn spec(secret: &str) -> Arc<GitAuthSpec> {
        Arc::new(
            GitAuthSpec::new(
                GitAuthKind::Basic,
                Some("alice".into()),
                SecretString::new(secret),
            )
            .unwrap(),
        )
    }

    fn ask(socket: &Path, nonce: &str, lines: &str) -> String {
        let mut out = Vec::new();
        request_via_socket(socket, nonce, "get", &mut lines.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn accepted_cache_request_is_revoked_and_releases_the_credential_on_drop() {
        use std::os::unix::net::UnixStream;
        let root = tempfile::tempdir().unwrap();
        let credential = spec("test-secret-revocation");
        let weak = Arc::downgrade(&credential);
        let listener = CacheSocketListener::start(
            &root.path().join("run"),
            credential,
            CredentialScope::for_http_url("https://git.example/r.git").unwrap(),
        )
        .unwrap();
        let mut client = UnixStream::connect(listener.socket()).unwrap();
        client.write_all(b"action=get\n").unwrap();
        awman_test_support::wait_for(
            "the cache request was accepted",
            Duration::from_secs(2),
            || listener.has_active_request().then_some(()),
        );
        let began = std::time::Instant::now();
        drop(listener);
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(weak.upgrade().is_none());
        let _ = client.write_all(b"protocol=https\nhost=git.example\npath=r.git\n");
        let _ = client.shutdown(std::net::Shutdown::Write);
        let mut response = String::new();
        let _ = client.read_to_string(&mut response);
        assert!(response.is_empty());
    }

    #[test]
    fn host_socket_answers_a_live_nonce_for_its_scope_only_and_redacts() {
        let root = tempfile::tempdir().unwrap();
        let server = CredentialSocketServer::start(
            &root.path().join("run"),
            HelperProgram::custom("/bin/false".into(), vec![]),
        )
        .unwrap();
        let scope = CredentialScope::for_http_url("https://git.example/o/r.git").unwrap();
        let ticket = server.ticket(spec("test-secret-socket"), scope);
        let nonce = ticket.nonce.clone();
        let good = "protocol=https\nhost=git.example\npath=o/r.git\n\n";
        assert_eq!(
            ask(server.path(), &nonce, good),
            "username=alice\npassword=test-secret-socket\n"
        );
        for other in [
            "protocol=https\nhost=other.example\npath=o/r.git\n\n",
            "protocol=https\nhost=git.example\npath=o/other.git\n\n",
            "protocol=http\nhost=git.example\npath=o/r.git\n\n",
            "protocol=https\nhost=git.example\n\n",
        ] {
            assert_eq!(ask(server.path(), &nonce, other), "", "{other}");
        }
        assert_eq!(ask(server.path(), &"0".repeat(32), good), "");
        let args = ticket.git_config_args().join(" ");
        assert!(args.contains(&nonce) && !args.contains("test-secret"));
        assert!(!format!("{:?}", ticket.env()).contains("test-secret"));
        assert_eq!(
            ticket.redact("x test-secret-socket YWxpY2U6dGVzdC1zZWNyZXQtc29ja2V0"),
            "x [redacted] [redacted]"
        );
        drop(ticket);
        assert_eq!(server.live_tickets(), 0);
        assert_eq!(ask(server.path(), &nonce, good), "");
        let mode = |p: &Path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode(server.path()), 0o600);
        assert_eq!(mode(server.path().parent().unwrap()), 0o700);
    }

    #[test]
    fn cache_listener_answers_get_for_the_session_host_and_ignores_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let scope = CredentialScope::for_http_url("http://127.0.0.1:9/o/r.git").unwrap();
        let listener =
            CacheSocketListener::start(&root.path().join("run"), spec("test-secret-cache"), scope)
                .unwrap();
        let ask = |body: &str| {
            String::from_utf8(imp::exchange(listener.socket(), body.as_bytes()).unwrap()).unwrap()
        };
        let want = "username=alice\npassword=test-secret-cache\n";
        assert_eq!(
            ask("action=get\ntimeout=900\nprotocol=http\nhost=127.0.0.1:9\n"),
            ""
        );
        assert_eq!(
            ask("action=get\ntimeout=900\nprotocol=http\nhost=127.0.0.1:9\npath=o/r.git\n"),
            want
        );
        for refused in [
            "action=get\ntimeout=900\nprotocol=http\nhost=evil.example\n",
            "action=get\ntimeout=900\nprotocol=http\nhost=127.0.0.1:9\npath=o/x.git\n",
            "action=store\ntimeout=900\nprotocol=http\nhost=127.0.0.1:9\nusername=a\npassword=b\n",
            "action=erase\nprotocol=http\nhost=127.0.0.1:9\n",
            "action=exit\n",
        ] {
            assert_eq!(ask(refused), "", "{refused}");
        }
        let dir = listener.dir().to_path_buf();
        drop(listener);
        drop(root);
        assert!(!dir.exists());
    }
}
