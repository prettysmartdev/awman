//! A disposable TLS endpoint forwards read-only requests to the granted real
//! Engine. Certificates and listener belong to one sandbox; no daemon config
//! or ambient Docker credentials are changed.
use awman::data::config::image_source::DockerTlsConfig;
use awman_test_support::TestSandbox;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

pub struct Endpoint {
    pub host: String,
    pub tls: DockerTlsConfig,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Endpoint {
    pub fn start(s: &TestSandbox, socket: &Path, mutual: bool) -> Self {
        Self::start_for(s, Upstream::Unix(socket.to_path_buf()), mutual, None)
    }

    pub fn start_tcp(s: &TestSandbox, addr: SocketAddr, authorization: Option<String>) -> Self {
        Self::start_for(s, Upstream::Tcp(addr), false, authorization)
    }

    fn start_for(
        s: &TestSandbox,
        upstream: Upstream,
        mutual: bool,
        authorization: Option<String>,
    ) -> Self {
        let listener = s.ports().tcp_listener();
        let addr = listener.local_addr().unwrap();
        let server = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let client = rcgen::generate_simple_self_signed(vec!["awman-test-client".into()]).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = if mutual {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(client.cert.der().clone()).unwrap();
            builder.with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::new(roots),
                    provider,
                )
                .build()
                .unwrap(),
            )
        } else {
            builder.with_no_client_auth()
        };
        let config = Arc::new(
            builder
                .with_single_cert(
                    vec![server.cert.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(server.signing_key.serialize_der())
                        .into(),
                )
                .unwrap(),
        );
        let cert_dir = s.root().join(if mutual { "mtls" } else { "tls" });
        std::fs::create_dir_all(&cert_dir).unwrap();
        let ca = cert_dir.join("ca.pem");
        let cert = cert_dir.join("client.pem");
        let key = cert_dir.join("client-key.pem");
        std::fs::write(&ca, server.cert.pem()).unwrap();
        std::fs::write(&cert, client.cert.pem()).unwrap();
        std::fs::write(&key, client.signing_key.serialize_pem()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                let stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let connection = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, stream);
                // Authentication failures deliberately produce no Engine request.
                let _ = forward(&mut stream, &upstream, authorization.as_deref());
            }
        });
        Self {
            host: format!("tcp://{addr}"),
            tls: DockerTlsConfig {
                ca,
                cert: mutual.then_some(cert),
                key: mutual.then_some(key),
                verify: true,
            },
            addr,
            stop,
            thread: Some(thread),
        }
    }
}
enum Upstream {
    Unix(std::path::PathBuf),
    Tcp(SocketAddr),
}
trait Connection: Read + Write {}
impl<T: Read + Write> Connection for T {}

fn forward(
    client: &mut (impl Read + Write),
    target: &Upstream,
    authorization: Option<&str>,
) -> std::io::Result<()> {
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= 16384 {
            return Err(std::io::Error::other("oversized request"));
        }
        client.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    let header = String::from_utf8(header).map_err(std::io::Error::other)?;
    if let Some(expected) = authorization {
        let supplied = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.trim());
        if supplied != Some(expected) {
            client.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"private-registry\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            return client.flush();
        }
    }
    let first = header.lines().next().unwrap_or("");
    if !first.starts_with("GET ") {
        return Err(std::io::Error::other("read-only Engine endpoint"));
    }
    let mut upstream: Box<dyn Connection> = match target {
        Upstream::Unix(socket) => {
            let stream = UnixStream::connect(socket)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            Box::new(stream)
        }
        Upstream::Tcp(addr) => {
            let stream = TcpStream::connect_timeout(addr, Duration::from_secs(10))?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            Box::new(stream)
        }
    };
    writeln!(upstream, "{first}\r")?;
    for line in header.lines().skip(1).filter(|line| !line.is_empty()) {
        if !line.to_ascii_lowercase().starts_with("connection:") {
            writeln!(upstream, "{line}\r")?;
        }
    }
    upstream.write_all(b"Connection: close\r\n\r\n")?;
    std::io::copy(&mut upstream, client)?;
    client.flush()
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
