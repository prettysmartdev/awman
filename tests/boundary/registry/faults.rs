//! Fault injection in front of the granted real registry. Image content still
//! comes from the registry; only transport/authentication responses are changed.
use super::client;
use awman_test_support::TestSandbox;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

#[derive(Clone, Copy)]
pub enum Fault {
    BearerExpiry,
    Unavailable(usize),
    DisconnectBlob,
    CorruptBlob,
    StallBlob,
}
pub struct Endpoint {
    pub addr: SocketAddr,
    pub requests: Arc<AtomicUsize>,
    pub blobs: Arc<AtomicUsize>,
    pub tokens: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    release: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}
impl Endpoint {
    pub fn start(s: &TestSandbox, upstream: SocketAddr, fault: Fault) -> Self {
        let listener = s.ports().tcp_listener();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let blobs = Arc::new(AtomicUsize::new(0));
        let tokens = Arc::new(AtomicUsize::new(0));
        let (release, hold) = mpsc::channel();
        let (done, seen, reads, issued) = (
            stop.clone(),
            requests.clone(),
            blobs.clone(),
            tokens.clone(),
        );
        let thread = std::thread::spawn(move || {
            for connection in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = connection.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut raw = Vec::new();
                let mut byte = [0];
                while !raw.ends_with(b"\r\n\r\n") && raw.len() < 16384 {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    raw.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&raw);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let count = seen.fetch_add(1, Ordering::SeqCst) + 1;
                let authorization = request
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim());
                if matches!(fault, Fault::BearerExpiry) {
                    if path.starts_with("/token?") {
                        if authorization != Some("Basic YXdtYW4tdGVzdC11c2VyOnRlc3Qtc2VjcmV0LXJlZ2lzdHJ5LXBhc3N3b3Jk") {
                            let _ = respond(&mut stream, 401, &[], b"");
                            continue;
                        }
                        let token = issued.fetch_add(1, Ordering::SeqCst) + 1;
                        let body = format!("{{\"token\":\"test-secret-token-{token}\"}}");
                        let _ = respond(
                            &mut stream,
                            200,
                            &[("Content-Type", "application/json")],
                            body.as_bytes(),
                        );
                        continue;
                    }
                    let expired = path.contains("/blobs/")
                        && authorization == Some("Bearer test-secret-token-1");
                    if authorization.is_none() || expired {
                        let challenge = format!("Bearer realm=\"http://{addr}/token\",service=\"awman-test\",scope=\"repository:awman-conformance/image:pull\"");
                        let _ = respond(&mut stream, 401, &[("WWW-Authenticate", &challenge)], b"");
                        continue;
                    }
                }
                if matches!(fault, Fault::Unavailable(n) if count <= n) {
                    let _ = respond(&mut stream, 503, &[], b"temporary registry outage");
                    continue;
                }
                let blob = if path.contains("/blobs/") {
                    reads.fetch_add(1, Ordering::SeqCst) + 1
                } else {
                    0
                };
                if blob > 0 && matches!(fault, Fault::StallBlob) {
                    // The test releases this owned server after the production
                    // deadline/cancellation interrupts its blocked body read.
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
                    );
                    let _ = hold.recv_timeout(Duration::from_secs(15));
                    continue;
                }
                let headers: Vec<_> = request
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .filter(|(name, _)| name.eq_ignore_ascii_case("accept"))
                    .map(|(name, value)| (name, value.trim()))
                    .collect();
                let response = client::request(upstream, "GET", path, &headers, b"").unwrap();
                if blob == 1 && matches!(fault, Fault::DisconnectBlob) {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&response.body[..response.body.len() / 2]);
                    continue;
                }
                let mut body = response.body;
                if blob > 0 && matches!(fault, Fault::CorruptBlob) && !body.is_empty() {
                    body[0] ^= 1;
                }
                let headers: Vec<_> = response
                    .headers
                    .iter()
                    .filter(|(name, _)| {
                        !["content-length", "transfer-encoding", "connection"]
                            .iter()
                            .any(|h| name.eq_ignore_ascii_case(h))
                    })
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let _ = respond(&mut stream, response.status, &headers, &body);
            }
        });
        Self {
            addr,
            requests,
            blobs,
            tokens,
            stop,
            release,
            thread: Some(thread),
        }
    }
}
fn respond(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} Registry\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(body)
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.release.send(());
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
