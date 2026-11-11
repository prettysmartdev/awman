//! A smart-HTTP git server on an owned loopback listener: every request is
//! handed to a supervised `git http-backend` (CGI) over the repositories
//! under one directory (typically the bare remotes of
//! [`crate::fixtures::Repo::bare_remote`]). Pushes are accepted.

use crate::http::{HttpServer, Request, Response};
use crate::process::{OwnedCommand, Stdin};
use crate::sandbox::Shared;
use crate::{EnvMap, TestSandbox};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest response body served (in-memory capture limit).
const MAX_RESPONSE: usize = 64 << 20;

/// A running smart-HTTP git server; stops when dropped.
pub struct FakeGitHttp {
    server: HttpServer,
    root: PathBuf,
}

impl FakeGitHttp {
    /// Serve every repository under `project_root`.
    pub fn start(sandbox: &TestSandbox, project_root: &Path) -> FakeGitHttp {
        let listener = sandbox.ports().tcp_listener();
        let shared = sandbox.shared().clone();
        let env = sandbox.env().clone();
        let root = project_root.to_path_buf();
        let handler_root = root.clone();
        let server = HttpServer::start(
            listener,
            Arc::new(move |req| handle(&shared, &env, &handler_root, req)),
        );
        FakeGitHttp { server, root }
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
