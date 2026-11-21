//! An owned authenticated HTTP proxy: CONNECT tunnels HTTPS unchanged and
//! absolute-form GET forwards HTTP. It can reach only this test's registry.
use awman_test_support::TestSandbox;
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};
pub struct Proxy {
    pub url: String,
    pub accepted: Arc<AtomicUsize>,
    pub tunnels: Arc<AtomicUsize>,
    pub forwarded: Arc<AtomicUsize>,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Proxy {
    pub fn start(s: &TestSandbox, target: SocketAddr) -> Self {
        let listener = s.ports().tcp_listener();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicUsize::new(0));
        let tunnels = Arc::new(AtomicUsize::new(0));
        let forwarded = Arc::new(AtomicUsize::new(0));
        let (done, seen, connects, gets) = (
            stop.clone(),
            accepted.clone(),
            tunnels.clone(),
            forwarded.clone(),
        );
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                let mut client = stream.unwrap();
                client
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                client
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                seen.fetch_add(1, Ordering::SeqCst);
                let mut header = Vec::new();
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") && header.len() < 16384 {
                    if client.read_exact(&mut byte).is_err() {
                        break;
                    }
                    header.push(byte[0]);
                }
                let header = String::from_utf8_lossy(&header);
                let authenticated =
                    header
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .any(|(name, value)| {
                            name.eq_ignore_ascii_case("proxy-authorization")
                                && value.trim()
                                    == "Basic dGVzdC11c2VyOnRlc3Qtc2VjcmV0LXByb3h5LXBhc3N3b3Jk"
                        });
                if !authenticated {
                    let _ = client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test-proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let first = header.lines().next().unwrap_or("");
                let mut words = first.split_whitespace();
                let method = words.next().unwrap_or("");
                let destination = words.next().unwrap_or("");
                let mut upstream =
                    TcpStream::connect_timeout(&target, Duration::from_secs(5)).unwrap();
                upstream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                upstream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                if method == "CONNECT" {
                    assert_eq!(
                        destination,
                        target.to_string(),
                        "proxy attempted another host"
                    );
                    connects.fetch_add(1, Ordering::SeqCst);
                    if client
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .is_err()
                    {
                        continue;
                    }
                    let mut from = client.try_clone().unwrap();
                    let mut to = upstream.try_clone().unwrap();
                    let outgoing = std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut to);
                        let _ = to.shutdown(Shutdown::Write);
                    });
                    let _ = std::io::copy(&mut upstream, &mut client);
                    let _ = client.shutdown(Shutdown::Write);
                    outgoing.join().unwrap();
                } else {
                    assert_eq!(method, "GET");
                    let prefix = format!("http://{target}");
                    let path = destination
                        .strip_prefix(&prefix)
                        .expect("proxy attempted another host");
                    gets.fetch_add(1, Ordering::SeqCst);
                    let _ = write!(upstream, "GET {path} HTTP/1.1\r\n");
                    for line in header.lines().skip(1).filter(|line| !line.is_empty()) {
                        if !line
                            .to_ascii_lowercase()
                            .starts_with("proxy-authorization:")
                            && !line.to_ascii_lowercase().starts_with("connection:")
                        {
                            let _ = write!(upstream, "{line}\r\n");
                        }
                    }
                    let _ = upstream.write_all(b"Connection: close\r\n\r\n");
                    let _ = std::io::copy(&mut upstream, &mut client);
                }
            }
        });
        Self {
            url: format!("http://test-user:test-secret-proxy-password@{addr}"),
            accepted,
            tunnels,
            forwarded,
            addr,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
