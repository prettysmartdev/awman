//! A smart-HTTP git server on an owned loopback listener: every request is
//! handed to a supervised `git http-backend` (CGI) over the repositories
//! under one directory (typically the bare remotes of
//! [`crate::fixtures::Repo::bare_remote`]). Pushes are accepted.
//!
//! [`GitHttpBuilder::fault`] makes chosen requests misbehave at the
//! transport level (no response, a stall, a dropped response after the push
//! was applied) to exercise retry and timeout classification.

use crate::http::{ConnectionFault, HttpServer, Request, Response};
use crate::process::{OwnedCommand, Stdin};
use crate::sandbox::Shared;
use crate::{EnvMap, TestSandbox};
use base64::Engine as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Largest response body served (in-memory capture limit).
const MAX_RESPONSE: usize = 64 << 20;
/// Upper bound of a stalled connection; it ends earlier when the client leaves.
const STALL_BOUND: Duration = Duration::from_secs(60);

/// A running smart-HTTP git server; stops when dropped.
pub struct FakeGitHttp {
    server: HttpServer,
    root: PathBuf,
    requests: Arc<Mutex<Vec<Option<String>>>>,
    receive_packs: Arc<AtomicUsize>,
}

/// What a fault rule sees of one request (never headers or bodies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHttpRequest {
    /// 0-based arrival order over the server's lifetime.
    pub index: usize,
    /// `GET` or `POST`.
    pub method: String,
    /// The request path, e.g. `/origin.git/info/refs`.
    pub path: String,
    /// The raw query, e.g. `service=git-receive-pack`.
    pub query: String,
}

impl GitHttpRequest {
    /// `git-upload-pack` (clone, fetch, `ls-remote`) or `git-receive-pack`
    /// (push), from the advertisement query or the RPC path.
    pub fn service(&self) -> Option<&str> {
        let from_query = self
            .query
            .split('&')
            .find_map(|pair| pair.strip_prefix("service="));
        from_query.or_else(|| {
            ["git-upload-pack", "git-receive-pack"]
                .into_iter()
                .find(|rpc| self.path.ends_with(&format!("/{rpc}")))
        })
    }

    /// The receive-pack RPC that applies a push.
    pub fn is_push_rpc(&self) -> bool {
        self.method == "POST" && self.path.ends_with("/git-receive-pack")
    }
}

/// How one request misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHttpFault {
    /// Read the request, then close the connection without a response byte
    /// (curl: "Empty reply from server").
    DropBeforeResponse,
    /// Read the request and send nothing until the client gives up.
    StallBeforeResponse,
    /// Send status and headers, then stall without the body (a timeout
    /// *after* a response started).
    StallAfterHeaders,
    /// Run the backend (a push is applied), then close the connection
    /// without returning its response: the accepted-then-reset case.
    ApplyThenDrop,
    /// Answer `401` with a Basic challenge whatever the request carries: a
    /// credential that expired or was revoked mid-session.
    RefuseCredentials,
}

type FaultRule = Arc<dyn Fn(&GitHttpRequest) -> Option<GitHttpFault> + Send + Sync>;

/// Optional HTTP authentication and faults; secrets never enter request observations.
pub struct GitHttpBuilder<'a> {
    sandbox: &'a TestSandbox,
    root: PathBuf,
    auth: Option<String>,
    fault: Option<FaultRule>,
    close_connections: bool,
}

impl GitHttpBuilder<'_> {
    /// Exercise clients against a server that closes every normal response.
    pub fn close_connections(mut self) -> Self {
        self.close_connections = true;
        self
    }
    /// Require this Basic identity; register the password with sandbox redaction.
    pub fn basic_auth(mut self, username: &str, password: &str) -> Self {
        self.sandbox.redact(password);
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        self.sandbox.redact(&encoded);
        self.auth = Some(format!("Basic {encoded}"));
        self
    }

    /// Decide per request whether it misbehaves; normal responses retain keep-alive.
    pub fn fault(
        mut self,
        rule: impl Fn(&GitHttpRequest) -> Option<GitHttpFault> + Send + Sync + 'static,
    ) -> Self {
        self.fault = Some(Arc::new(rule));
        self
    }

    /// Drop the first `n` requests before any response.
    pub fn fail_before_response(self, n: usize) -> Self {
        self.fault(move |r| (r.index < n).then_some(GitHttpFault::DropBeforeResponse))
    }

    /// Apply the first push, then drop its response.
    pub fn drop_after_receive_pack(self) -> Self {
        let dropped = AtomicUsize::new(0);
        self.fault(move |r| {
            (r.is_push_rpc() && dropped.fetch_add(1, Ordering::SeqCst) == 0)
                .then_some(GitHttpFault::ApplyThenDrop)
        })
    }

    /// Start the server on a sandbox-owned loopback listener.
    pub fn start(self) -> FakeGitHttp {
        let listener = self.sandbox.ports().tcp_listener();
        let shared = self.sandbox.shared().clone();
        let env = self.sandbox.env().clone();
        let root = self.root;
        let handler_root = root.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let receive_packs = Arc::new(AtomicUsize::new(0));
        let applied = receive_packs.clone();
        let arrivals = AtomicUsize::new(0);
        let server = HttpServer::start(
            listener,
            Arc::new(move |req| {
                let view = GitHttpRequest {
                    index: arrivals.fetch_add(1, Ordering::SeqCst),
                    method: req.method.clone(),
                    path: req.path.clone(),
                    query: req.query.clone(),
                };
                let authorization = req.header("authorization");
                let user = authorization
                    .and_then(|a| a.strip_prefix("Basic "))
                    .and_then(|a| base64::engine::general_purpose::STANDARD.decode(a).ok())
                    .and_then(|a| String::from_utf8(a).ok())
                    .and_then(|a| a.split_once(':').map(|(u, _)| u.to_owned()));
                observed.lock().unwrap().push(user);
                let fault = self.fault.as_ref().and_then(|rule| rule(&view));
                match fault {
                    Some(GitHttpFault::DropBeforeResponse) => {
                        return Response::new(500).fault(ConnectionFault::Drop)
                    }
                    Some(GitHttpFault::StallBeforeResponse) => {
                        return Response::new(500).fault(ConnectionFault::Stall(STALL_BOUND))
                    }
                    Some(GitHttpFault::RefuseCredentials) => {
                        return Response::new(401)
                            .header("WWW-Authenticate", "Basic realm=\"awman-test\"")
                            .header("Connection", "close")
                    }
                    _ => {}
                }
                let response = if self
                    .auth
                    .as_deref()
                    .is_some_and(|a| authorization != Some(a))
                {
                    Response::new(401).header("WWW-Authenticate", "Basic realm=\"awman-test\"")
                } else {
                    let response = handle(&shared, &env, &handler_root, req);
                    if view.is_push_rpc() && response.status == 200 {
                        applied.fetch_add(1, Ordering::SeqCst);
                    }
                    response
                };
                match fault {
                    Some(GitHttpFault::ApplyThenDrop) => response.fault(ConnectionFault::Drop),
                    Some(GitHttpFault::StallAfterHeaders) => {
                        response.fault(ConnectionFault::HeadersThenStall(STALL_BOUND))
                    }
                    _ if self.close_connections => response.header("Connection", "close"),
                    _ => response,
                }
            }),
        );
        FakeGitHttp {
            server,
            root,
            requests,
            receive_packs,
        }
    }
}

impl FakeGitHttp {
    /// Serve every repository under `project_root`.
    pub fn start(sandbox: &TestSandbox, project_root: &Path) -> FakeGitHttp {
        Self::builder(sandbox, project_root).start()
    }

    /// Configure authentication and faults before starting the owned HTTP listener.
    pub fn builder<'a>(sandbox: &'a TestSandbox, project_root: &Path) -> GitHttpBuilder<'a> {
        GitHttpBuilder {
            sandbox,
            root: project_root.to_path_buf(),
            auth: None,
            fault: None,
            close_connections: false,
        }
    }

    /// Observed Basic-auth usernames only. Passwords and headers are never retained.
    pub fn requests(&self) -> Vec<Option<String>> {
        self.requests.lock().unwrap().clone()
    }

    /// Push RPCs the backend ran successfully (including one whose response
    /// was then dropped by [`GitHttpFault::ApplyThenDrop`]).
    pub fn receive_packs(&self) -> usize {
        self.receive_packs.load(Ordering::SeqCst)
    }

    /// `http://127.0.0.1:<port>/<repository>` (e.g. `origin.git`).
    pub fn url(&self, repository: &str) -> String {
        format!("http://{}/{repository}", self.server.addr())
    }

    /// Its address.
    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    /// The served directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn handle(shared: &Arc<Shared>, env: &EnvMap, root: &Path, req: Request) -> Response {
    if req.path.contains("..") {
        return Response::new(400).body("bad path");
    }
    let mut command = OwnedCommand::new(
        shared.clone(),
        "git".as_ref(),
        env.clone(),
        root.to_path_buf(),
    )
    .arg("http-backend")
    .label("git http-backend")
    .max_output(MAX_RESPONSE)
    .env("GIT_PROJECT_ROOT", root)
    .env("GIT_HTTP_EXPORT_ALL", "1")
    .env("PATH_INFO", &req.path)
    .env("REQUEST_METHOD", &req.method)
    .env("QUERY_STRING", &req.query)
    .env("CONTENT_LENGTH", req.body.len().to_string())
    .env("REMOTE_ADDR", "127.0.0.1")
    .env("REMOTE_USER", "awman-test");
    if let Some(v) = req.header("content-type") {
        command = command.env("CONTENT_TYPE", v);
    }
    if let Some(v) = req.header("content-encoding") {
        command = command.env("HTTP_CONTENT_ENCODING", v);
    }
    if let Some(v) = req.header("git-protocol") {
        command = command.env("GIT_PROTOCOL", v);
    }
    let output = command.stdin(Stdin::Bytes(req.body)).run();
    if !output.exit.success() {
        return Response::new(500).body(output.stderr);
    }
    parse_cgi(&output.stdout)
}

/// Split CGI output into a response.
fn parse_cgi(raw: &[u8]) -> Response {
    let (head, body) = match find(raw, b"\r\n\r\n") {
        Some(at) => (&raw[..at], &raw[at + 4..]),
        None => match find(raw, b"\n\n") {
            Some(at) => (&raw[..at], &raw[at + 2..]),
            None => (raw, &[][..]),
        },
    };
    let mut response = Response::new(200);
    for line in String::from_utf8_lossy(head).lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("status") {
            response.status = v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(500);
        } else {
            response = response.header(k, v);
        }
    }
    response.body(body.to_vec())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgi_status_headers_and_body() {
        let r = parse_cgi(b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nnope");
        assert_eq!(r.status, 404);
        assert_eq!(r.get_header("content-type"), Some("text/plain"));
        assert_eq!(r.body, b"nope");
        let r = parse_cgi(b"Content-Type: x\n\nbody");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"body");
    }
}
