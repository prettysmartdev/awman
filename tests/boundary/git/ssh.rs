//! SSH: an unprivileged `sshd` on loopback serving the sandbox's bare
//! remotes to one generated client key. The client is awman's host git with
//! `GIT_SSH_COMMAND` pinned to that key and a private known-hosts file, so
//! nothing of the developer's SSH setup is read.
//!
//! The refusing remote is the same server addressed as a user it does not
//! allow: authentication fails immediately (`BatchMode=yes`, no prompt).

use super::world::Transport;
use awman_test_support::capability::GitTransports;
use awman_test_support::{Granted, OwnedChild, Ready, TestSandbox};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub struct SshServer {
    addr: SocketAddr,
    user: String,
    closed: SocketAddr,
    /// Kept so the server is stopped (and reaped) when the world is dropped.
    _sshd: OwnedChild,
}

/// A fresh keypair at `path` (and `path.pub`).
fn keygen(sandbox: &TestSandbox, keygen: &Path, path: &Path) {
    sandbox
        .command(keygen)
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "awman-boundary",
            "-f",
        ])
        .arg(path)
        .label("ssh-keygen")
        .run()
        .assert_success();
}

fn tool(name: &str) -> PathBuf {
    ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("BLOCKED[git-transports]: {name} was not found"))
}

impl SshServer {
    /// Generate keys, write the server configuration, start `sshd` on an
    /// owned loopback port and point the sandbox's git at it.
    pub fn start(sandbox: &mut TestSandbox, transports: &Granted<GitTransports>) -> SshServer {
        let dir = sandbox.root().join("sshd");
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", dir.display()));
        let keygen_exe = tool("ssh-keygen");
        let host_key = dir.join("host_key");
        let client_key = dir.join("client_key");
        keygen(sandbox, &keygen_exe, &host_key);
        keygen(sandbox, &keygen_exe, &client_key);
        let authorized = dir.join("authorized_keys");
        std::fs::copy(dir.join("client_key.pub"), &authorized)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: authorized_keys: {e}"));

        let user = sandbox
            .command(tool("id"))
            .arg("-un")
            .label("id")
            .run()
            .stdout_text()
            .trim()
            .to_string();
        assert!(
            !user.is_empty(),
            "BLOCKED[git-transports]: cannot name the login user"
        );

        let addr = {
            let listener = sandbox.ports().tcp_listener();
            listener.local_addr().expect("listener address")
        };
        let closed = {
            let listener = sandbox.ports().tcp_listener();
            listener.local_addr().expect("listener address")
        };
        let config = dir.join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {host}\nPidFile {pid}\n\
                 AuthorizedKeysFile {auth}\nAllowUsers {user}\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nUsePAM no\n\
                 StrictModes no\nLogLevel ERROR\n",
                port = addr.port(),
                host = host_key.display(),
                pid = dir.join("sshd.pid").display(),
                auth = authorized.display(),
            ),
        )
        .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: sshd_config: {e}"));

        let sshd = sandbox
            .command(&transports.sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .label("sshd")
            .ready(Ready::TcpAccepts(addr))
            .spawn();

        let known_hosts = dir.join("known_hosts");
        sandbox.env_mut().set(
            "GIT_SSH_COMMAND",
            format!(
                "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=no \
                 -o UserKnownHostsFile={} -o ConnectTimeout=10",
                client_key.display(),
                known_hosts.display()
            ),
        );
        SshServer {
            addr,
            user,
            closed,
            _sshd: sshd,
        }
    }
}

impl Transport for SshServer {
    fn subject(&self) -> String {
        "SSH (sshd on loopback)".to_string()
    }

    fn url(&self, bare: &Path) -> String {
        format!(
            "ssh://{}@{}:{}{}",
            self.user,
            self.addr.ip(),
            self.addr.port(),
            bare.display()
        )
    }

    fn refused_url(&self, bare: &Path) -> Option<String> {
        Some(format!(
            "ssh://awman-not-allowed@{}:{}{}",
            self.addr.ip(),
            self.addr.port(),
            bare.display()
        ))
    }

    fn unreachable_url(&self) -> String {
        format!(
            "ssh://{}@{}:{}/nowhere.git",
            self.user,
            self.closed.ip(),
            self.closed.port()
        )
    }
}
