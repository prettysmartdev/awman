//! Loopback doubles and image builders for `engine::oci` component tests.
//!
//! Every server listens on an address the sandbox owns (`sandbox.ports()`:
//! a tracked 127.0.0.1:0 listener or a Unix socket in the short socket
//! directory), serves from a thread that blocks in `accept` (no polling),
//! and is stopped on drop by a wake-up connection; the socket file is
//! removed so the sandbox leak check stays clean. Nothing contacts a real
//! daemon, registry, credential store or the developer's HOME.

// Shared by several tests; not every helper is used on every platform.
#![allow(dead_code)]

use awman::data::config::env::EnvSnapshot;
use awman::data::oci_identity::OciPlatform;
use awman::engine::oci::{AcquireLimits, CachingAcquirer, DiskSpace, RetryPolicy};
use awman_test_support::TestSandbox;
use sha2::{Digest as _, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

// ── Small helpers ───────────────────────────────────────────────────────────

pub struct PlentyOfSpace;

impl DiskSpace for PlentyOfSpace {
    fn available(&self, _path: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn limits() -> AcquireLimits {
    AcquireLimits {
        max_archive_bytes: 256 * 1024 * 1024,
        max_layer_bytes: 64 * 1024 * 1024,
        max_layers: 64,
        min_free_bytes: 0,
    }
}

pub fn arm64() -> OciPlatform {
    OciPlatform {
        os: "linux".into(),
        architecture: "arm64".into(),
        variant: None,
    }
}

/// An acquirer on `state` with `env`, three attempts, and no real backoff.
pub fn acquirer(state: &Path, env: EnvSnapshot) -> CachingAcquirer {
    CachingAcquirer::new(state, limits(), env, Arc::new(PlentyOfSpace))
        .with_retry(RetryPolicy {
            max_attempts: 3,
            deadline: Duration::from_secs(10),
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        })
        .with_sleeper(Arc::new(|_| {}))
}

/// Files in `<state>/oci-cache/<dir>` (0 when the directory is absent).
pub fn cache_entries(state: &Path, dir: &str) -> usize {
    std::fs::read_dir(state.join("oci-cache").join(dir))
        .map(|d| d.count())
        .unwrap_or(0)
}

// ── Image archives ──────────────────────────────────────────────────────────

/// One uncompressed layer with regular files `(path, bytes, mode)`.
pub fn layer(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data, mode) in files {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_size(data.len() as u64);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap()
}

fn image_config(platform: &OciPlatform, layers: &[Vec<u8>]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "created": "2026-01-01T00:00:00Z",
        "os": platform.os,
        "architecture": platform.architecture,
        "config": {
            "User": "1000:1000",
            "Env": ["PATH=/usr/local/bin:/usr/bin:/bin", "HOME=/home/agent"],
            "WorkingDir": "/workspace",
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": layers.iter().map(|l| format!("sha256:{}", hex(l))).collect::<Vec<_>>(),
        },
        "history": layers.iter().map(|_| serde_json::json!({"created_by": "fixture"})).collect::<Vec<_>>(),
    }))
    .unwrap()
}

fn append(outer: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    outer.append_data(&mut header, path, bytes).unwrap();
}

/// A legacy `docker save` archive of one image named `name`.
pub fn docker_save(platform: &OciPlatform, layers: &[Vec<u8>], name: &str) -> Vec<u8> {
    let mut outer = tar::Builder::new(Vec::new());
    let mut paths = Vec::new();
    for (i, raw) in layers.iter().enumerate() {
        let id = hex(&[hex(raw).as_bytes(), &[i as u8]].concat());
        append(&mut outer, &format!("{id}/VERSION"), b"1.0");
        append(&mut outer, &format!("{id}/json"), br#"{"id":"x"}"#);
        append(&mut outer, &format!("{id}/layer.tar"), raw);
        paths.push(format!("{id}/layer.tar"));
    }
    let config = image_config(platform, layers);
    let config_name = format!("{}.json", hex(&config));
    append(&mut outer, &config_name, &config);
    let manifest =
        serde_json::json!([{ "Config": config_name, "RepoTags": [name], "Layers": paths }]);
    append(
        &mut outer,
        "manifest.json",
        &serde_json::to_vec(&manifest).unwrap(),
    );
    let (repo, tag) = name.rsplit_once(':').unwrap();
    append(
        &mut outer,
        "repositories",
        &serde_json::to_vec(&serde_json::json!({ repo: { tag: "x" } })).unwrap(),
    );
    outer.into_inner().unwrap()
}

/// The archive the Docker Engine double exports for `awman-x-claude:latest`.
pub fn agent_archive() -> Vec<u8> {
    docker_save(
        &arm64(),
        &[layer(&[("bin/agent", b"x", 0o755)])],
        "awman-x-claude:latest",
    )
}

// ── HTTP replies ────────────────────────────────────────────────────────────

/// One HTTP response. `missing` announces more bytes than are sent
/// (a truncated body); `close_early` drops the connection after the head.
pub struct Reply {
    pub status: u16,
    pub media: &'static str,
    pub body: Vec<u8>,
    pub missing: usize,
}

impl Reply {
    pub fn body(body: impl Into<Vec<u8>>, media: &'static str) -> Reply {
        Reply {
            status: 200,
            media,
            body: body.into(),
            missing: 0,
        }
    }

    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            ..Reply::body("", "text/plain")
        }
    }
}

type Route = dyn Fn(&str) -> Reply + Send + Sync;

/// Read one request head; returns its path (`None` on a closed or refused
/// connection, including a client's TLS refusal).
fn read_request(stream: &mut impl Read) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return None,
        }
    }
    let text = String::from_utf8(head).ok()?;
    text.split_whitespace().nth(1).map(str::to_string)
}

fn write_reply(stream: &mut impl Write, reply: &Reply) {
    let head = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.media,
        reply.body.len() + reply.missing,
    );
    // A rejected or cancelled client may already have closed its socket.
    if stream.write_all(head.as_bytes()).is_ok() {
        let _ = stream.write_all(&reply.body);
        let _ = stream.flush();
    }
}

// ── TCP server (plain or TLS) on a sandbox-owned listener ───────────────────

/// A one-connection-at-a-time HTTP(S) server: every connection serves one
/// request and closes, like the registry and Engine doubles need.
pub struct HttpFixture {
    pub addr: SocketAddr,
    /// PEM of the self-signed `localhost` certificate (TLS servers).
    pub cert: String,
    pub accepts: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HttpFixture {
    pub fn start(
        sandbox: &TestSandbox,
        tls: bool,
        route: impl Fn(&str) -> Reply + Send + Sync + 'static,
    ) -> HttpFixture {
        let listener: TcpListener = sandbox.ports().tcp_listener();
        let addr = listener.local_addr().unwrap();
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = key.cert.pem();
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![key.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into(),
            )
            .unwrap(),
        );
        let route: Arc<Route> = Arc::new(route);
        let stop = Arc::new(AtomicBool::new(false));
        let accepts = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let (done, count, seen) = (stop.clone(), accepts.clone(), paths.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { return };
                count.fetch_add(1, Ordering::SeqCst);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                if tls {
                    let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                    serve(rustls::StreamOwned::new(conn, stream), &*route, &seen);
                } else {
                    serve(stream, &*route, &seen);
                }
            }
        });
        HttpFixture {
            addr,
            cert,
            accepts,
            paths,
            stop,
            thread: Some(thread),
        }
    }

    /// How many requests named exactly `path`.
    pub fn count(&self, path: &str) -> usize {
        self.paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == path)
            .count()
    }

    pub fn accepted(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }
}

fn serve(mut stream: impl Read + Write, route: &Route, paths: &Mutex<Vec<String>>) {
    let Some(path) = read_request(&mut stream) else {
        return;
    };
    paths.lock().unwrap().push(path.clone());
    write_reply(&mut stream, &route(&path));
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept; the server sees `stop` and returns.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ── Docker Engine double on a Unix socket ───────────────────────────────────

/// `(status, body, truncate_to)` for a request path.
pub type EngineHandler = dyn Fn(&str) -> (u16, Vec<u8>, Option<usize>) + Send + Sync;

/// A minimal HTTP/1.1 Docker Engine on a socket in the sandbox socket dir.
/// Records every request path. Keep-alive connections are served until the
/// client closes them.
#[cfg(unix)]
pub struct EngineDouble {
    pub socket: PathBuf,
    pub seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl EngineDouble {
    /// hygiene-allow(H4): Socket path is leased by TestSandbox Ports; EngineDouble drop wakes and joins the listener thread.
    pub fn start(sandbox: &TestSandbox, name: &str, handler: Arc<EngineHandler>) -> EngineDouble {
        let socket = sandbox.ports().unix_socket(name);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, done) = (seen.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { return };
                let handler = handler.clone();
                let log = log.clone();
                std::thread::spawn(move || engine_connection(stream, &*handler, &log));
            }
        });
        EngineDouble {
            socket,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    pub fn paths(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    /// Stop serving and remove the socket (the daemon "disappears").
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.stop.store(true, Ordering::SeqCst);
            let _ = std::os::unix::net::UnixStream::connect(&self.socket);
            let _ = thread.join();
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

#[cfg(unix)]
impl Drop for EngineDouble {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
fn engine_connection(
    stream: std::os::unix::net::UnixStream,
    handler: &EngineHandler,
    log: &Mutex<Vec<String>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut out = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                break;
            }
        }
        log.lock().unwrap().push(path.clone());
        let (status, body, truncate) = handler(&path);
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = out.write_all(head.as_bytes());
        match truncate {
            Some(n) => {
                let _ = out.write_all(&body[..n]);
                return;
            }
            None => {
                let _ = out.write_all(&body);
            }
        }
    }
}

/// The standard Engine: `_ping`, `version` (`api`), inspect of
/// `awman-x-claude:latest` reporting `os_arch`, and its export (optionally
/// truncated half-way); everything else is 404 "No such image".
pub fn standard_engine(
    api: &'static str,
    os_arch: (&'static str, &'static str),
    archive: Vec<u8>,
    truncate: bool,
) -> Arc<EngineHandler> {
    Arc::new(move |path: &str| {
        let p = path.split('?').next().unwrap_or("");
        match p {
            "/_ping" => (200, b"OK".to_vec(), None),
            "/version" => (
                200,
                format!(r#"{{"ApiVersion":"{api}"}}"#).into_bytes(),
                None,
            ),
            _ if p.ends_with("/images/awman-x-claude:latest/json") => (
                200,
                format!(
                    r#"{{"Os":"{}","Architecture":"{}","Size":1000}}"#,
                    os_arch.0, os_arch.1
                )
                .into_bytes(),
                None,
            ),
            _ if p.ends_with("/images/awman-x-claude:latest/get") => {
                let n = archive.len() / 2;
                (200, archive.clone(), truncate.then_some(n))
            }
            _ => (404, br#"{"message":"No such image"}"#.to_vec(), None),
        }
    })
}
