//! Local wire fixtures for source classification, exercised through acquisition.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{DockerTlsConfig, ImageSourceSpec, RegistryHostConfig};
use crate::data::fs::hash::sha256_hex;
use crate::engine::error::EngineError;

use super::archive::tests::{arm64, limits};
use super::verify::tests::FixedDisk;
use super::{AcquirePolicy, AcquireRequest, CachingAcquirer, ImageAcquirer, RetryPolicy};

const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

struct Reply {
    status: u16,
    media: &'static str,
    body: Vec<u8>,
    missing: usize,
}
impl Reply {
    fn body(body: impl Into<Vec<u8>>, media: &'static str) -> Self {
        Self {
            status: 200,
            media,
            body: body.into(),
            missing: 0,
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::body("", "text/plain")
        }
    }
}

struct Server {
    addr: SocketAddr,
    accepts: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    cert: String,
}
impl Server {
    fn start(tls: bool, route: impl Fn(&str) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = key.cert.pem();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![key.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into(),
        )
        .unwrap();
        let config = Arc::new(config);
        let stop = Arc::new(AtomicBool::new(false));
        let accepts = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let (done, count, seen) = (stop.clone(), accepts.clone(), paths.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                count.fetch_add(1, Ordering::SeqCst);
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                if tls {
                    let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                    respond(rustls::StreamOwned::new(conn, stream), &route, &seen);
                } else {
                    respond(stream, &route, &seen);
                }
            }
        });
        Self {
            addr,
            accepts,
            paths,
            stop,
            thread: Some(thread),
            cert,
        }
    }
    fn count(&self, path: &str) -> usize {
        self.paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == path)
            .count()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn respond(
    mut stream: impl Read + Write,
    route: &impl Fn(&str) -> Reply,
    paths: &Mutex<Vec<String>>,
) {
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") && header.len() < 16 * 1024 {
        match stream.read(&mut byte) {
            Ok(1) => header.push(byte[0]),
            _ => return, // Includes the client's TLS refusal.
        }
    }
    let text = String::from_utf8(header).unwrap();
    let path = text.split_whitespace().nth(1).unwrap();
    paths.lock().unwrap().push(path.into());
    let reply = route(path);
    let header = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status, reply.media, reply.body.len() + reply.missing,
    );
    // A rejected or cancelled client may have closed its socket.
    if stream.write_all(header.as_bytes()).is_ok() {
        let _ = stream.write_all(&reply.body);
        let _ = stream.flush();
    }
}

fn request(server: &Server, reference: &str, tls: bool) -> AcquireRequest {
    let host = server.addr.to_string();
    AcquireRequest {
        tag: "awman-test:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some(host.clone()),
            reference: Some(reference.into()),
        },
        platform: arm64(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::from([(
            host,
            RegistryHostConfig {
                insecure: !tls,
                ca_cert: None,
                auth: None,
            },
        )]),
    }
}
fn acquire(state: &std::path::Path, request: &AcquireRequest) -> EngineError {
    let err = CachingAcquirer::new(
        state,
        limits(),
        EnvSnapshot::empty(),
        Arc::new(FixedDisk(None)),
    )
    .with_retry(RetryPolicy {
        deadline: Duration::from_secs(10),
        initial_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
        ..RetryPolicy::DEFAULT
    })
    .with_sleeper(Arc::new(|_| {}))
    .acquire(request, &mut |_| {})
    .unwrap_err();
    for dir in ["oci-cache/tmp", "oci-cache/images"] {
        let path = state.join(dir);
        if path.exists() {
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 0, "{dir}: {err}");
        }
    }
    err
}
fn manifest(config: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": MANIFEST,
        "config": {"digest": format!("sha256:{}", sha256_hex(config)), "size": config.len()},
        "layers": [],
    }))
    .unwrap()
}

#[test]
fn registry_malformed_json_schema_and_digest_fields_are_final() {
    for case in [
        "probe-json",
        "index-json",
        "index-schema",
        "manifest-json",
        "manifest-schema",
        "config-json",
        "config-schema",
        "index-digest",
        "config-digest",
    ] {
        let server = Server::start(false, move |path| {
            if path.contains("/blobs/") {
                return Reply::body(
                    if case == "config-json" { "{" } else { "{}" },
                    "application/json",
                );
            }
            match case {
                "probe-json" => Reply::body("{", "application/json"),
                "index-json" => Reply::body("{", INDEX),
                "index-schema" => Reply::body("{}", INDEX),
                "manifest-json" => Reply::body("{", MANIFEST),
                "manifest-schema" => Reply::body("{}", MANIFEST),
                "index-digest" => Reply::body(
                    serde_json::to_vec(&serde_json::json!({
                        "manifests": [{"digest": "invalid", "size": 1,
                            "platform": {"os": "linux", "architecture": "arm64"}}]
                    }))
                    .unwrap(),
                    INDEX,
                ),
                "config-digest" => Reply::body(
                    br#"{"config":{"digest":"invalid","size":1},"layers":[]}"#.to_vec(),
                    MANIFEST,
                ),
                _ => Reply::body(
                    manifest(if case == "config-json" { b"{" } else { b"{}" }),
                    MANIFEST,
                ),
            }
        });
        let state = tempfile::tempdir().unwrap();
        let err = acquire(state.path(), &request(&server, "team/tls-agent:1", false));
        assert!(matches!(err, EngineError::Network(_)), "{case}: {err}");
        assert_eq!(
            server.count("/v2/team/tls-agent/manifests/1"),
            1,
            "{case}: {err}"
        );
    }
}

#[test]
fn registry_digest_and_platform_mismatches_are_final() {
    for platform in [false, true] {
        let server = Server::start(false, move |_| {
            if platform {
                Reply::body(br#"{"manifests":[{"digest":"unused","size":1,"platform":{"os":"linux","architecture":"s390x"}}]}"#.to_vec(), INDEX)
            } else {
                // A digest-pinned reference cannot resolve to these bytes.
                Reply::body("{}", MANIFEST)
            }
        });
        let state = tempfile::tempdir().unwrap();
        let reference = if platform {
            "team/agent:1".into()
        } else {
            format!("team/agent@sha256:{}", "f".repeat(64))
        };
        let err = acquire(state.path(), &request(&server, &reference, false));
        assert!(
            matches!(
                err,
                EngineError::ImagePlatformMismatch { .. } | EngineError::ImageDigestMismatch { .. }
            ),
            "{err}"
        );
        assert_eq!(server.accepts.load(Ordering::SeqCst), 1, "{err}");
    }
}

#[test]
fn registry_status_and_truncation_attempt_counts_ignore_tls_in_reference() {
    for reference in ["team/agent:1", "team/tls-agent:1"] {
        for status in [400, 401, 403, 404, 500, 503] {
            let server = Server::start(false, move |_| Reply::status(status));
            let state = tempfile::tempdir().unwrap();
            let err = acquire(state.path(), &request(&server, reference, false));
            assert_eq!(
                server.accepts.load(Ordering::SeqCst),
                if status >= 500 { 3 } else { 1 },
                "{status}: {err}"
            );
            if [401, 403].contains(&status) {
                assert!(matches!(err, EngineError::Auth(_)));
            }
        }
        for blob in [false, true] {
            let server = Server::start(false, move |path| {
                if blob && path.contains("/manifests/") {
                    Reply::body(
                        manifest(br#"{"os":"linux","architecture":"arm64"}"#),
                        MANIFEST,
                    )
                } else {
                    Reply {
                        missing: 100,
                        ..Reply::body("{", MANIFEST)
                    }
                }
            });
            let state = tempfile::tempdir().unwrap();
            let err = acquire(state.path(), &request(&server, reference, false));
            let repository = reference.trim_end_matches(":1");
            assert_eq!(
                server.count(&format!("/v2/{repository}/manifests/1")),
                3,
                "{err}"
            );
        }
    }
}

#[test]
fn docker_metadata_decoding_and_status_have_typed_attempt_counts() {
    for at_inspect in [false, true] {
        for (status, body, attempts) in [(200, "{", 1), (200, "{}", 1), (403, "", 1), (503, "", 3)]
        {
            let server = Server::start(false, move |path| {
                if path == "/_ping" {
                    return Reply::body("OK", "text/plain");
                }
                if at_inspect && path == "/version" {
                    return Reply::body(br#"{"ApiVersion":"1.45"}"#.to_vec(), "application/json");
                }
                Reply {
                    status,
                    ..Reply::body(body, "application/json")
                }
            });
            let state = tempfile::tempdir().unwrap();
            let mut req = request(&server, "team/tls-agent:1", false);
            req.source = ImageSourceSpec::DockerStore {
                host: Some(format!("tcp://{}", server.addr)),
                tls: None,
                reference: Some("tls-agent:latest".into()),
            };
            let err = acquire(state.path(), &req);
            assert_eq!(
                server.count("/_ping"),
                attempts,
                "inspect={at_inspect} {status} {body}: {err}"
            );
        }
    }
}

#[test]
fn tls_refusals_stop_after_one_connection_for_both_sources() {
    for docker in [false, true] {
        let server = Server::start(true, |_| panic!("untrusted TLS must fail before HTTP"));
        let state = tempfile::tempdir().unwrap();
        let mut req = request(&server, "team/tls-agent:1", true);
        if docker {
            let ca = state.path().join("ca.pem");
            std::fs::write(&ca, &server.cert).unwrap();
            req.source = ImageSourceSpec::DockerStore {
                host: Some(format!("tcp://{}", server.addr)),
                tls: Some(DockerTlsConfig {
                    ca,
                    cert: None,
                    key: None,
                    verify: true,
                }),
                reference: Some("tls-agent:latest".into()),
            };
        }
        // Registry rejects an unknown issuer; Docker trusts the issuer but
        // rejects the certificate's localhost name for 127.0.0.1.
        let err = acquire(state.path(), &req);
        assert!(matches!(err, EngineError::Network(_)), "{err}");
        assert_eq!(
            server.accepts.load(Ordering::SeqCst),
            1,
            "docker={docker}: {err}"
        );
    }
}

#[test]
fn proxy_auth_refusal_is_final_without_inspecting_its_diagnostic() {
    use super::retry::{run_bounded, CancelToken, Deadline, OperationControl};
    use super::transport::Transport;
    use crate::engine::error::ImageRetryClass;

    let server = Server::start(false, |_| Reply::status(407));
    let client = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::https(format!("http://{}", server.addr)).unwrap())
        .build()
        .unwrap();
    let cancel = CancelToken::new();
    let deadline = Deadline::start(Duration::from_secs(10));
    let transport = Transport::new(OperationControl::new(cancel.clone(), deadline)).unwrap();
    let mut attempts = 0;
    let err = run_bounded(RetryPolicy::DEFAULT, deadline, &cancel, &|_| {}, |_| {
        attempts += 1;
        transport
            .send(client.get("https://registry.example.invalid/v2/"))
            .map(|_| ())
            .map_err(|e| {
                e.map_http(|e| ImageRetryClass::http(&e).failure("changed diagnostic".into()))
            })
    })
    .unwrap_err();
    assert_eq!(attempts, 1, "{err}");
    assert_eq!(server.accepts.load(Ordering::SeqCst), 1);
}
