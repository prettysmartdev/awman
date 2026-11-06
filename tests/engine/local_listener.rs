//! The daemon's local listener and its client connector (WI 0140).
//!
//! Unix only: socket mode/owner checks, `server.json` that grants nothing
//! when missing, stale or tampered, stale-socket removal only under the
//! startup claim, refusal with no daemon, and no TCP listener. The wrong-UID
//! peer refusal is a unit test in `src/frontend/api/local.rs` (it needs to
//! tell the listener a different owner uid, which is not public API).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use awman::data::fs::daemon_paths::DaemonPaths;
use awman::data::fs::daemon_sidecar::{
    ServerSidecar, SidecarTransport, SidecarTransportKind, PROTOCOL_API,
};
use awman::engine::daemon::{AcquireError, DaemonClaim, DaemonGuard};
use awman::engine::remote::local::{self, LocalClient};
use awman::frontend::api::local::{start_claimed, LocalTransport};

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    paths: DaemonPaths,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("daemon");
        let paths = DaemonPaths::new(&root);
        Self {
            _dir: dir,
            root,
            paths,
        }
    }
    fn endpoint(&self) -> PathBuf {
        self.paths.local_endpoint()
    }
    fn pid_file(&self) -> PathBuf {
        self.paths.pid_file()
    }
    fn sidecar(&self) -> PathBuf {
        self.paths.server_meta_file()
    }
    /// The startup claim for this test process, exactly as the daemon takes it.
    fn claim(&self) -> DaemonClaim {
        self.paths.ensure_root().unwrap();
        DaemonGuard::new(self.paths.clone())
            .acquire(std::process::id())
            .unwrap()
    }
    fn write_pid(&self, pid: u32) {
        self.paths.ensure_root().unwrap();
        std::fs::write(self.pid_file(), pid.to_string()).unwrap();
    }
    fn write_sidecar(&self, pid: u32, address: &Path) {
        ServerSidecar::current(
            pid,
            SidecarTransport {
                kind: SidecarTransportKind::UnixSocket,
                address: address.display().to_string(),
            },
        )
        .write(&self.sidecar())
        .unwrap();
    }
    fn discover(&self) -> Result<Option<local::LocalEndpoint>, awman::engine::error::EngineError> {
        local::discover(&self.paths)
    }
}

/// A live process whose name says "awman", so the guard and `discover` treat
/// its pid as a running daemon (the test binary itself is not named awman).
struct FakeDaemon {
    _dir: tempfile::TempDir,
    child: std::process::Child,
}

impl FakeDaemon {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sleep = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .expect("sleep binary");
        let binary = dir.path().join("awman-fake");
        std::fs::copy(sleep, &binary).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Some Linux hosts provide sleep through a multi-call binary that
        // dispatches by argv[0]. macOS `ps comm` observes argv[0], so leave
        // the executable name intact there for the daemon identity check.
        let mut command = std::process::Command::new(&binary);
        #[cfg(target_os = "linux")]
        command.arg0("sleep");
        command.arg("60");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let child = loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("spawn fake daemon: {error}"),
            }
        };
        Self { _dir: dir, child }
    }
    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn router() -> axum::Router {
    axum::Router::new().route(
        "/v1/health",
        axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
    )
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test]
async fn local_listener_socket_is_0600_in_a_private_dir_and_serves_the_owner() {
    let home = Home::new();
    let claim = home.claim();
    let (_listener, bound) = start_claimed(&claim, &home.endpoint(), router()).unwrap();
    claim.finish();
    assert_eq!(bound.transport, LocalTransport::UnixSocket);
    assert_eq!(bound.address, home.endpoint().display().to_string());
    assert_eq!(mode(&home.endpoint()), 0o600);
    assert_eq!(mode(&home.root) & 0o077, 0);
    let response = LocalClient::new(home.endpoint())
        .send("GET", "/v1/health", None)
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body["status"], "ok");
}

#[tokio::test]
async fn local_listener_dropping_removes_the_socket() {
    let home = Home::new();
    let claim = home.claim();
    let (listener, _) = start_claimed(&claim, &home.endpoint(), router()).unwrap();
    drop(claim);
    assert!(home.endpoint().exists());
    drop(listener);
    assert!(!home.endpoint().exists());
}

#[tokio::test]
async fn local_client_refuses_cleanly_when_no_daemon_runs() {
    let home = Home::new();
    assert!(home.discover().unwrap().is_none());
    let error = LocalClient::new(home.endpoint())
        .send("GET", "/v1/health", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(local::NO_DAEMON), "{error}");

    // A leftover socket nobody listens on is the same answer.
    awman::data::fs::private_files::create_private_dir(&home.root).unwrap();
    drop(std::os::unix::net::UnixListener::bind(home.endpoint()).unwrap());
    std::fs::set_permissions(home.endpoint(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let error = LocalClient::new(home.endpoint())
        .send("GET", "/v1/health", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(local::NO_DAEMON), "{error}");
}

#[tokio::test]
async fn local_client_refuses_an_open_socket_a_plain_file_or_an_open_directory() {
    let home = Home::new();
    let claim = home.claim();
    let (_listener, _) = start_claimed(&claim, &home.endpoint(), router()).unwrap();
    let client = LocalClient::new(home.endpoint()).with_timeout(Duration::from_secs(5));

    std::fs::set_permissions(home.endpoint(), std::fs::Permissions::from_mode(0o666)).unwrap();
    let error = client.send("GET", "/v1/health", None).await.unwrap_err();
    assert!(
        error.to_string().contains("accessible to other users"),
        "{error}"
    );
    std::fs::set_permissions(home.endpoint(), std::fs::Permissions::from_mode(0o600)).unwrap();

    std::fs::set_permissions(&home.root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = client.send("GET", "/v1/health", None).await.unwrap_err();
    assert!(error.to_string().contains("not private"), "{error}");
    std::fs::set_permissions(&home.root, std::fs::Permissions::from_mode(0o700)).unwrap();

    let plain = home.root.join("plain.sock");
    std::fs::write(&plain, b"").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o600)).unwrap();
    let error = LocalClient::new(plain)
        .send("GET", "/v1/health", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not a socket"), "{error}");

    // Restored: served again.
    assert_eq!(
        client.send("GET", "/v1/health", None).await.unwrap().status,
        200
    );
}

#[tokio::test]
async fn server_json_missing_stale_or_tampered_grants_nothing() {
    let home = Home::new();
    let claim = home.claim();
    let (_listener, _) = start_claimed(&claim, &home.endpoint(), router()).unwrap();
    drop(claim);
    // The "daemon" whose pid the files name.
    let daemon = FakeDaemon::spawn();
    let live = daemon.pid();

    // No pidfile: nothing, even with a sidecar present.
    std::fs::remove_file(home.pid_file()).unwrap();
    home.write_sidecar(live, &home.endpoint());
    assert!(home.discover().unwrap().is_none());

    // Live pidfile, missing sidecar: nothing.
    home.write_pid(live);
    ServerSidecar::remove(&home.sidecar()).unwrap();
    assert!(home.discover().unwrap().is_none());

    // Stale: the sidecar's pid is not the live pidfile's.
    home.write_sidecar(dead_pid(), &home.endpoint());
    assert!(home.discover().unwrap().is_none());

    // Stale: the pidfile names a dead process.
    let dead = dead_pid();
    home.write_pid(dead);
    home.write_sidecar(dead, &home.endpoint());
    assert!(home.discover().unwrap().is_none());

    // Not a daemon: a live pid that is not an awman process.
    let me = std::process::id();
    home.write_pid(me);
    home.write_sidecar(me, &home.endpoint());
    assert!(home.discover().unwrap().is_none());
    home.write_pid(live);

    // Tampered address: refused, not followed.
    home.write_sidecar(live, Path::new("/tmp/elsewhere.sock"));
    let error = home.discover().unwrap_err();
    assert!(error.to_string().contains("unexpected endpoint"), "{error}");

    // Tampered protocol identity: refused with an actionable message.
    let mut sidecar = ServerSidecar::current(
        live,
        SidecarTransport {
            kind: SidecarTransportKind::UnixSocket,
            address: home.endpoint().display().to_string(),
        },
    );
    sidecar.protocol.insert(PROTOCOL_API.into(), 999);
    sidecar.write(&home.sidecar()).unwrap();
    let error = home.discover().unwrap_err();
    assert!(
        error.to_string().contains("different API protocol"),
        "{error}"
    );

    // Wrong transport kind for this platform.
    sidecar.protocol.insert(PROTOCOL_API.into(), 1);
    sidecar.transport.kind = SidecarTransportKind::NamedPipe;
    sidecar.write(&home.sidecar()).unwrap();
    assert!(home.discover().is_err());

    // Tampered permissions: refused.
    home.write_sidecar(live, &home.endpoint());
    std::fs::set_permissions(home.sidecar(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(home.discover().is_err());

    // Garbage: refused.
    std::fs::set_permissions(home.sidecar(), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(home.sidecar(), b"{\"pid\":").unwrap();
    assert!(home.discover().is_err());

    // Valid: discovered, and the sidecar is metadata only (no key/token field).
    home.write_sidecar(live, &home.endpoint());
    let endpoint = home.discover().unwrap().expect("live daemon");
    assert_eq!(endpoint.address, home.endpoint());
    assert_eq!(endpoint.pid, live);
    let raw = std::fs::read_to_string(home.sidecar()).unwrap();
    let fields: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let mut names: Vec<_> = fields.as_object().unwrap().keys().cloned().collect();
    names.sort();
    assert_eq!(
        names,
        ["pid", "protocol", "started_at", "transport", "version"]
    );
    let response = LocalClient::new(endpoint.address)
        .send("GET", "/v1/health", None)
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn stale_socket_is_removed_only_under_the_startup_claim() {
    let home = Home::new();
    home.paths.ensure_root().unwrap();
    // A stale socket: bound, then its server went away.
    drop(std::os::unix::net::UnixListener::bind(home.endpoint()).unwrap());
    assert!(home.endpoint().exists());

    // A live daemon pidfile blocks the claim, so nothing can remove the socket.
    let daemon = FakeDaemon::spawn();
    home.write_pid(daemon.pid());
    let blocked = DaemonGuard::new(home.paths.clone()).acquire(std::process::id());
    assert!(
        matches!(blocked, Err(AcquireError::AlreadyRunning { pid }) if pid == daemon.pid()),
        "{:?}",
        blocked.err()
    );
    assert!(home.endpoint().exists());
    drop(daemon);

    // A claim whose pidfile was taken over may not remove it either.
    let claim = home.claim();
    home.write_pid(dead_pid());
    assert!(start_claimed(&claim, &home.endpoint(), router()).is_err());
    assert!(home.endpoint().exists());
    drop(claim);

    // Dead pidfile, fresh claim: the stale socket is replaced.
    let claim = home.claim();
    let (_listener, _) = start_claimed(&claim, &home.endpoint(), router()).unwrap();
    claim.finish();
    let response = LocalClient::new(home.endpoint())
        .send("GET", "/v1/health", None)
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn start_never_replaces_a_live_server_or_a_foreign_file() {
    let home = Home::new();
    let claim = home.claim();
    let (_first, _) = start_claimed(&claim, &home.endpoint(), router()).unwrap();
    let error = start_claimed(&claim, &home.endpoint(), router())
        .err()
        .expect("a live server is not stale");
    assert!(error.to_string().contains("already serving"), "{error}");
    drop(claim);

    let other = Home::new();
    other.paths.ensure_root().unwrap();
    std::fs::write(other.endpoint(), b"not a socket").unwrap();
    let claim = other.claim();
    let error = start_claimed(&claim, &other.endpoint(), router())
        .err()
        .expect("foreign file refused");
    assert!(error.to_string().contains("not a socket"), "{error}");
    assert_eq!(std::fs::read(other.endpoint()).unwrap(), b"not a socket");
}

#[tokio::test]
async fn start_fails_closed_when_the_directory_cannot_be_private() {
    let home = Home::new();
    let claim = home.claim();
    let root = home.root.join("shared");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(start_claimed(&claim, &root.join("api.sock"), router()).is_err());
    assert!(!root.join("api.sock").exists());
}

/// Listening TCP sockets held by this process, by inode (Linux `/proc`).
#[cfg(target_os = "linux")]
fn own_tcp_listeners() -> std::collections::BTreeSet<String> {
    let mut listening = std::collections::BTreeSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() > 9 && cols[3] == "0A" {
                listening.insert(cols[9].to_string());
            }
        }
    }
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter_map(|target| {
            let target = target.to_string_lossy().to_string();
            let inode = target
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .to_string();
            listening.contains(&inode).then_some(inode)
        })
        .collect()
}

const NO_TCP_CHILD: &str = "AWMAN_TEST_LOCAL_LISTENER_NO_TCP_CHILD";

/// Runs alone in a child process (other tests in this binary bind TCP).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn local_listener_no_tcp_child() {
    if std::env::var_os(NO_TCP_CHILD).is_none() {
        return;
    }
    let before = own_tcp_listeners();
    let home = Home::new();
    let claim = home.claim();
    let (_listener, _) = start_claimed(&claim, &home.endpoint(), router()).unwrap();
    claim.finish();
    let response = LocalClient::new(home.endpoint())
        .send("GET", "/v1/health", None)
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
        own_tcp_listeners(),
        before,
        "the local listener opened a TCP port"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn local_listener_opens_no_tcp_port() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "local_listener::local_listener_no_tcp_child",
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(NO_TCP_CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed"), "child did not run: {stdout}");
}
