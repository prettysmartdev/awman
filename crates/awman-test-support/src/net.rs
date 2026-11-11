//! Owned network endpoints.
//!
//! No function returns a "free port number": a listener is bound to port 0
//! and handed over, or a child binds port 0 and reports its address (seam
//! S4), which the test then [`Ports::track`]s. Every tracked endpoint must be
//! closed when the test ends (leak check).

use crate::sandbox::Shared;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Longest socket path accepted (macOS `sockaddr_un.sun_path` is 104 bytes
/// including the terminating NUL; Linux allows 108).
pub const MAX_SOCKET_PATH: usize = 103;

/// How long a closed listener may stay reachable (fork-to-exec window of a
/// concurrent spawn) before it counts as leaked.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Owned ports and Unix sockets of one sandbox ([`crate::TestSandbox::ports`]).
pub struct Ports {
    shared: Arc<Shared>,
    sock_dir: PathBuf,
}

impl Ports {
    pub(crate) fn new(shared: Arc<Shared>, sock_dir: PathBuf) -> Ports {
        Ports { shared, sock_dir }
    }

    /// A listener on `127.0.0.1:0`, tracked for the leak check. Hand it (or
    /// its `local_addr()`) to the code under test.
    pub fn tcp_listener(&self) -> TcpListener {
        self.shared.check_may_use_network();
        // The one sanctioned bind site for tests: port 0, loopback, tracked.
        #[allow(clippy::disallowed_methods)]
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|e| panic!("BLOCKED[loopback]: cannot bind 127.0.0.1:0: {e}"));
        let addr = listener.local_addr().expect("listener address");
        self.track(addr);
        listener
    }

    /// A Unix-socket path inside the sandbox's short socket directory,
    /// tracked for the leak check. Nothing is created; the code under test
    /// binds it. Panics if the resulting path is too long for `sockaddr_un`.
    pub fn unix_socket(&self, name: &str) -> PathBuf {
        self.shared.check_may_use_network();
        assert!(
            !name.is_empty() && !name.contains(['/', '\\']),
            "socket name must be a plain file name: {name:?}"
        );
        let path = self.sock_dir.join(name);
        assert!(
            path.as_os_str().len() <= MAX_SOCKET_PATH,
            "socket path {} is {} bytes; sockaddr_un allows {MAX_SOCKET_PATH}",
            path.display(),
            path.as_os_str().len()
        );
        self.shared.sockets.lock().unwrap().push(path.clone());
        path
    }

    /// Track an address a child reported, so the leak check verifies that
    /// nothing accepts on it any more when the test ends.
    pub fn track(&self, addr: SocketAddr) {
        self.shared.tcp.lock().unwrap().push(addr);
    }

    /// The directory Unix sockets live in.
    pub fn sock_dir(&self) -> &Path {
        &self.sock_dir
    }
}

/// Leak check: tracked TCP addresses still accepting, socket files left.
pub(crate) fn leak_check(shared: &Shared, sock_dir: &Path) -> Vec<crate::Leak> {
    let mut leaks = Vec::new();
    for addr in shared.tcp.lock().unwrap().iter() {
        // A closed listener can stay reachable for a moment while another
        // thread of the test process is between fork and exec (the child
        // holds a duplicate until exec closes it). Only an address that keeps
        // accepting past the grace period is a leak.
        let closed = crate::wait::poll(CLOSE_GRACE, || {
            TcpStream::connect_timeout(addr, Duration::from_millis(200))
                .is_err()
                .then_some(())
        });
        if closed.is_none() {
            leaks.push(crate::Leak::new(
                "port",
                format!("{addr} still accepts connections"),
            ));
        }
    }
    if let Ok(entries) = std::fs::read_dir(sock_dir) {
        for entry in entries.flatten() {
            leaks.push(crate::Leak::new(
                "socket",
                format!("{} was not removed", entry.path().display()),
            ));
            let _ = std::fs::remove_file(entry.path());
        }
    }
    leaks
}
