//! A local bare repository served over Git's smart HTTP protocol (WI 0143).
//!
//! The worker end-to-end tests need a remote that behaves like a real Git
//! host: HTTP(S) URL, `401` until the client presents the lease credential,
//! and a record of what each request carried. `git http-backend` (shipped
//! with every Git) does the protocol work; this helper is the web server in
//! front of it, on loopback:
//!
//! - **Auth.** With a token set, every request must carry
//!   `Authorization: Basic base64(<any user>:<token>)` or
//!   `Authorization: Bearer <token>`; otherwise `401` with
//!   `WWW-Authenticate: Basic`, which is what makes Git ask its credential
//!   helper.
//! - **Phases.** The test labels the next requests (`set_phase("clone")`) so
//!   it can assert that clone, fetch, `ls-remote` and push each authenticated.
//! - **Faults.** `reset_next_connections(n)` drops the next `n` accepted TCP
//!   connections with a reset before any byte of a response (a pre-response
//!   transport failure); `reject_pushes()` installs a `pre-receive` hook that
//!   declines every push (a remote rejection).
//!
//! The secret never reaches the request log: [`SeenRequest`] keeps only
//! whether the request was authorised and which scheme it used.
//!
//! Included with `#[path = "../helpers/smart_http.rs"] mod smart_http;`
//! (or `#[path = "helpers/smart_http.rs"]` from a top-level test file).
//! Tests using it carry `real_git` in their names so `make test-fast` skips
//! them.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use base64::Engine as _;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// The identity fixture commits are made with, passed per command.
const IDENTITY: [&str; 4] = [
    "-c",
    "user.name=Fixture",
    "-c",
    "user.email=fixture@awman.test",
];

/// `git` with no global or system config.
pub fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

/// Run `git args` in `cwd`, panic on failure, return trimmed stdout.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = git_command(cwd).args(args).output().expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Commit everything in `dir` as the fixture identity.
pub fn commit_all(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    let mut args: Vec<&str> = IDENTITY.to_vec();
    args.extend(["commit", "-q", "-m", message]);
    git(dir, &args);
    git(dir, &["rev-parse", "HEAD"])
}

/// Whether this Git ships `git http-backend`.
pub fn http_backend_available() -> bool {
    http_backend().is_some()
}

/// Env switch that lets a real-Git suite skip when `git http-backend` is
/// missing. Without it the suite fails, so CI cannot pass vacuously.
pub const ALLOW_SKIP_REAL_GIT_ENV: &str = "AWMAN_TEST_ALLOW_SKIP_REAL_GIT";

/// `true` when `git http-backend` is available. Otherwise panics with a clear
/// message, unless [`ALLOW_SKIP_REAL_GIT_ENV`]`=1`, in which case it prints
/// why and returns `false` (the caller returns early).
pub fn require_http_backend() -> bool {
    if http_backend_available() {
        return true;
    }
    if std::env::var(ALLOW_SKIP_REAL_GIT_ENV).as_deref() == Ok("1") {
        eprintln!("skipping: git http-backend is not installed ({ALLOW_SKIP_REAL_GIT_ENV}=1)");
        return false;
    }
    panic!(
        "git http-backend is required by this real-Git test but `git --exec-path` has none; \
         install Git's http-backend, or set {ALLOW_SKIP_REAL_GIT_ENV}=1 to skip explicitly"
    );
}

fn http_backend() -> Option<PathBuf> {
    let output = Command::new("git").arg("--exec-path").output().ok()?;
    let exec = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let backend = exec.join("git-http-backend");
    (output.status.success() && backend.exists()).then_some(backend)
}

/// One HTTP request the server answered (no secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenRequest {
    pub phase: String,
    pub method: String,
    /// Path and query, e.g. `/repo.git/info/refs?service=git-upload-pack`.
    pub target: String,
    /// `basic`, `bearer`, `other` or `none`.
    pub auth_scheme: &'static str,
    pub authorized: bool,
    pub status: u16,
}

impl SeenRequest {
    pub fn is_push(&self) -> bool {
        self.target.contains("git-receive-pack")
    }
}

#[derive(Default)]
struct ServerState {
    token: Option<String>,
    phase: String,
    requests: Vec<SeenRequest>,
    reset_next: usize,
    resets: usize,
    /// Every `Authorization` value seen, kept only so a test can assert the
    /// token arrived in it; never printed.
    raw_authorizations: Vec<String>,
}

/// A served bare repository.
pub struct SmartHttpRepo {
    /// The directory `GIT_PROJECT_ROOT` points at.
    pub root: PathBuf,
    /// The bare repository (`<root>/repo.git`).
    pub bare: PathBuf,
    /// `http://127.0.0.1:<port>/repo.git`.
    pub url: String,
    pub addr: SocketAddr,
    state: Arc<Mutex<ServerState>>,
    handle: JoinHandle<()>,
}

impl Drop for SmartHttpRepo {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl SmartHttpRepo {
    /// Create `<dir>/repo.git` seeded with one commit on `main` (a README and
    /// `Dockerfile.dev`), then serve it. `token: None` serves anonymously.
    pub async fn start(dir: &Path, token: Option<&str>) -> Self {
        let root = dir.join("srv");
        let bare = root.join("repo.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-q", "-b", "main"]);
        // Allow pushes without REMOTE_USER checks being the only gate.
        git(&bare, &["config", "http.receivepack", "true"]);
        let seed = dir.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("README.md"), "# fixture\n").unwrap();
        std::fs::write(seed.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        commit_all(&seed, "seed");
        git(&seed, &["push", "-q", bare.to_str().unwrap(), "main"]);

        let state = Arc::new(Mutex::new(ServerState {
            token: token.map(str::to_owned),
            phase: "setup".into(),
            ..Default::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = http_backend().expect("git http-backend");
        let app = Router::new().fallback(any(handle)).with_state(Handler {
            state: state.clone(),
            root: root.clone(),
            backend,
        });
        let listener = ResettingListener {
            inner: listener,
            state: state.clone(),
        };
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://{addr}/repo.git"),
            root,
            bare,
            addr,
            state,
            handle,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ServerState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Label the requests that follow.
    pub fn set_phase(&self, phase: &str) {
        self.lock().phase = phase.into();
    }

    /// Accept a different token from now on (credential rotation).
    pub fn set_token(&self, token: Option<&str>) {
        self.lock().token = token.map(str::to_owned);
    }

    /// Drop the next `n` TCP connections with a reset before responding.
    pub fn reset_next_connections(&self, n: usize) {
        self.lock().reset_next = n;
    }

    /// How many connections were reset so far.
    pub fn resets(&self) -> usize {
        self.lock().resets
    }

    pub fn requests(&self) -> Vec<SeenRequest> {
        self.lock().requests.clone()
    }

    pub fn requests_in(&self, phase: &str) -> Vec<SeenRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.phase == phase)
            .collect()
    }

    /// Whether any `Authorization` header carried `needle` (decoded Basic
    /// included).
    pub fn saw_credential(&self, needle: &str) -> bool {
        self.lock()
            .raw_authorizations
            .iter()
            .any(|a| a.contains(needle))
    }

    /// Install a `pre-receive` hook that declines every push.
    pub fn reject_pushes(&self) {
        let hook = self.bare.join("hooks").join("pre-receive");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            "#!/bin/sh\necho 'policy: pushes are frozen' >&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// `refs/heads/<branch>` in the bare repository, if it exists.
    pub fn branch_sha(&self, branch: &str) -> Option<String> {
        let output = git_command(&self.bare)
            .args([
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{branch}"),
            ])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Create `branch` on the server from `main` plus one commit adding `file`.
    pub fn publish_branch(&self, scratch: &Path, branch: &str, file: &str) -> String {
        let work = scratch.join(format!("publish-{}", branch.replace('/', "-")));
        git(
            scratch,
            &[
                "clone",
                "-q",
                self.bare.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        std::fs::write(work.join(file), file).unwrap();
        let sha = commit_all(&work, &format!("add {file}"));
        git(
            &work,
            &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
        );
        sha
    }
}

#[derive(Clone)]
struct Handler {
    state: Arc<Mutex<ServerState>>,
    root: PathBuf,
    backend: PathBuf,
}

/// Classify `Authorization` against the expected token.
fn check_auth(headers: &HeaderMap, token: Option<&str>) -> (&'static str, bool, Option<String>) {
    let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return ("none", token.is_none(), None);
    };
    let (scheme, presented) = if let Some(b64) = value.strip_prefix("Basic ") {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        let password = decoded.split_once(':').map(|(_, p)| p.to_string());
        ("basic", password.map(|p| (p, decoded)))
    } else if let Some(t) = value.strip_prefix("Bearer ") {
        ("bearer", Some((t.trim().to_string(), value.to_string())))
    } else {
        ("other", None)
    };
    let raw = presented.as_ref().map(|(_, raw)| raw.clone());
    let ok = match token {
        None => true,
        Some(expected) => presented.as_ref().is_some_and(|(p, _)| p == expected),
    };
    (scheme, ok, raw)
}

async fn handle(State(h): State<Handler>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let method = parts.method.as_str().to_string();
    let (phase, token) = {
        let s = h.state.lock().unwrap_or_else(|p| p.into_inner());
        (s.phase.clone(), s.token.clone())
    };
    let (auth_scheme, authorized, raw) = check_auth(&parts.headers, token.as_deref());
    let record = |status: u16| {
        let mut s = h.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(raw) = &raw {
            s.raw_authorizations.push(raw.clone());
        }
        s.requests.push(SeenRequest {
            phase: phase.clone(),
            method: method.clone(),
            target: target.clone(),
            auth_scheme,
            authorized,
            status,
        });
    };
    if !authorized {
        record(401);
        let mut response = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        response.headers_mut().insert(
            "www-authenticate",
            HeaderValue::from_static("Basic realm=\"wsp-test\""),
        );
        return response;
    }
    let body = match to_bytes(body, 256 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            record(400);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let mut child = tokio::process::Command::new(&h.backend)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_PROJECT_ROOT", &h.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REMOTE_USER", "worker")
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("GIT_PROTOCOL", header("git-protocol"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git http-backend");
    let mut stdin = child.stdin.take().unwrap();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
        drop(stdin);
    });
    let output = child.wait_with_output().await.expect("git http-backend");
    let _ = writer.await;
    let (status, response) = cgi_response(&output.stdout);
    record(status);
    response
}

/// Turn a CGI response (headers, blank line, body) into an HTTP response.
fn cgi_response(raw: &[u8]) -> (u16, Response) {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
    let Some((end, sep)) = split else {
        return (502, StatusCode::BAD_GATEWAY.into_response());
    };
    let head = String::from_utf8_lossy(&raw[..end]).to_string();
    let body = raw[end + sep..].to_vec();
    let mut status = 200u16;
    let mut builder = Response::builder();
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            status = value
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(500);
        } else {
            builder = builder.header(name.trim(), value);
        }
    }
    let response = builder
        .status(status)
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    (status, response)
}

/// A listener that resets the next `reset_next` connections at accept.
struct ResettingListener {
    inner: TcpListener,
    state: Arc<Mutex<ServerState>>,
}

impl axum::serve::Listener for ResettingListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((stream, addr)) = self.inner.accept().await else {
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            };
            let reset = {
                let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if s.reset_next > 0 {
                    s.reset_next -= 1;
                    s.resets += 1;
                    true
                } else {
                    false
                }
            };
            if reset {
                // SO_LINGER 0: close sends RST, the client sees
                // "Connection reset by peer" before any response byte.
                #[allow(deprecated)]
                let _ = stream.set_linger(Some(Duration::ZERO));
                drop(stream);
                continue;
            }
            return (stream, addr);
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A Git credential helper script speaking the same file protocol as
/// awman's hidden `__git-credential-helper` (nonce and action as its last
/// two arguments, secret read from `$AWMAN_LEASE_CREDENTIAL_FILE`). It logs
/// each invocation's argv and the Git config isolation variables to
/// `<dir>/helper.log` so a test can assert no secret was passed on argv.
pub struct HelperScript {
    pub program: PathBuf,
    pub log: PathBuf,
}

impl HelperScript {
    pub fn install(dir: &Path) -> Self {
        std::fs::create_dir_all(dir).unwrap();
        let program = dir.join("lease-helper.sh");
        let log = dir.join("helper.log");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\n\
                 echo \"argv=$* nosystem=$GIT_CONFIG_NOSYSTEM global=$GIT_CONFIG_GLOBAL\" >> '{log}'\n\
                 [ \"$2\" = get ] || exit 0\n\
                 cat >/dev/null\n\
                 grep -q \"^nonce=$1\\$\" \"$AWMAN_LEASE_CREDENTIAL_FILE\" || exit 0\n\
                 grep -E '^(username|password)=' \"$AWMAN_LEASE_CREDENTIAL_FILE\"\n",
                log = log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self { program, log }
    }

    pub fn program(&self) -> awman::engine::git::lease_transport::HelperProgram {
        awman::engine::git::lease_transport::HelperProgram::custom(self.program.clone(), vec![])
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// Every file under `dir` (recursively) whose bytes contain `needle`.
pub fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else if meta.is_file() {
            if let Ok(bytes) = std::fs::read(&path) {
                if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    hits.push(path);
                }
            }
        }
    }
    hits
}
