//! A loopback TCP forwarder that counts the connections it accepts. The
//! real registry cannot say how many requests it served; every request awman
//! makes needs a connection through the forwarder, so an unchanged count
//! proves a client never contacted the registry.

use std::io::copy;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub struct CountingProxy {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CountingProxy {
    /// Forward connections accepted on `listener` to `upstream`.
    pub fn start(listener: TcpListener, upstream: SocketAddr) -> CountingProxy {
        let addr = listener.local_addr().expect("proxy listener address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, flag) = (accepted.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            for client in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(client) = client else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                let Ok(server) = TcpStream::connect_timeout(&upstream, Duration::from_secs(10))
                else {
                    continue;
                };
                forward(client, server);
            }
        });
        CountingProxy {
            addr,
            accepted,
            stop,
            thread: Some(thread),
        }
    }

    /// `ip:port` clients connect to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Connections accepted so far.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Stop accepting and release the listener. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it observes the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CountingProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

fn forward(client: TcpStream, server: TcpStream) {
    let pairs = [
        (client.try_clone(), server.try_clone()),
        (server.try_clone(), client.try_clone()),
    ];
    for (from, to) in pairs {
        let (Ok(mut from), Ok(mut to)) = (from, to) else {
            return;
        };
        std::thread::spawn(move || {
            let _ = copy(&mut from, &mut to);
            let _ = to.shutdown(Shutdown::Write);
        });
    }
}
