//! Smart HTTP: `git http-backend` (git's own CGI) behind a loopback HTTP
//! server, serving the bare remotes of the sandbox. The server has no
//! access control, so the "refusing remote" check is not applicable here;
//! the SSH transport covers it.

use super::world::Transport;
use awman_test_support::fakes::FakeGitHttp;
use awman_test_support::TestSandbox;
use std::net::TcpListener;
use std::path::Path;

pub struct SmartHttp {
    server: FakeGitHttp,
    /// A port nothing listens on any more (its listener was dropped).
    closed: std::net::SocketAddr,
}

impl SmartHttp {
    pub fn start(sandbox: &TestSandbox) -> SmartHttp {
        let remotes = sandbox.root().join("remotes");
        std::fs::create_dir_all(&remotes)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", remotes.display()));
        let server = FakeGitHttp::start(sandbox, &remotes);
        let closed = {
            let listener: TcpListener = sandbox.ports().tcp_listener();
            listener.local_addr().expect("listener address")
        };
        SmartHttp { server, closed }
    }
}

impl Transport for SmartHttp {
    fn subject(&self) -> String {
        "smart HTTP (git http-backend on loopback)".to_string()
    }

    fn url(&self, bare: &Path) -> String {
        let name = bare
            .file_name()
            .expect("bare repository name")
            .to_string_lossy()
            .into_owned();
        self.server.url(&name)
    }

    fn refused_url(&self, _bare: &Path) -> Option<String> {
        None
    }

    fn unreachable_url(&self) -> String {
        format!("http://{}/origin.git", self.closed)
    }
}
