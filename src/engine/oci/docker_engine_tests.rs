//! Hermetic component coverage for Docker connection diagnoses. All sockets
//! belong to the support sandbox; none use the host's Docker installation.
use super::*;
use crate::data::config::image_source::ImageSourceSpec;
use crate::engine::oci::{
    retry, AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer, DiskSpace, ImageAcquirer,
    RetryPolicy,
};
use awman_test_support::{TestCase, TestSandbox};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    deadline: Duration::from_secs(5),
    initial_backoff: Duration::ZERO,
    max_backoff: Duration::ZERO,
};

struct Space;
impl DiskSpace for Space {
    fn available(&self, _: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}
fn acquirer(s: &TestSandbox, env: EnvSnapshot) -> CachingAcquirer {
    CachingAcquirer::new(
        &s.work().join("state"),
        AcquireLimits {
            min_free_bytes: 0,
            ..AcquireLimits::default()
        },
        env,
        Arc::new(Space),
    )
    .with_retry(POLICY)
}
fn request(host: Option<String>) -> AcquireRequest {
    AcquireRequest {
        tag: "agent:latest".into(),
        source: ImageSourceSpec::DockerStore {
            host,
            tls: None,
            reference: None,
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::new(),
    }
}
fn ping(endpoint: &Endpoint) -> EngineError {
    let cancel = retry::CancelToken::new();
    let deadline = retry::Deadline::start(POLICY.deadline);
    let mut attempts = 0;
    let err = retry::run_bounded(POLICY, deadline, &cancel, &|_| {}, |_| {
        attempts += 1;
        Client::new(endpoint, OperationControl::new(cancel.clone(), deadline))?
            .get("/_ping", Duration::from_secs(1))
    })
    .err()
    .expect("fixture must fail");
    assert_eq!(
        attempts,
        if retry::is_retryable(&err) { 3 } else { 1 },
        "{err}"
    );
    err
}
fn assert_stopped(err: &EngineError, endpoint: &str) {
    assert!(
        matches!(
            local_unavailability(err),
            Some(LocalDockerUnavailable::Stopped { .. })
        ),
        "{err}"
    );
    assert!(!retry::is_retryable(err));
    let text = err.to_string();
    assert!(
        text.contains(&format!("Docker is not running at {endpoint}.")),
        "{text}"
    );
    assert!(
        text.contains("Start Docker, then run `awman ready` again."),
        "{text}"
    );
    assert!(!text.contains("after 3"), "{text}");
}

#[cfg(unix)]
#[test]
fn missing_socket_keeps_selected_env_and_explicit_endpoints_and_is_final() {
    let s = TestSandbox::for_test(TestCase::component("engine::oci::docker_engine::tests::missing_socket_keeps_selected_env_and_explicit_endpoints_and_is_final"));
    let env_host = format!("unix://{}", s.ports().unix_socket("env.sock").display());
    let explicit = format!(
        "unix://{}",
        s.ports().unix_socket("explicit.sock").display()
    );
    let env = EnvSnapshot::from_full_env([("DOCKER_HOST".into(), env_host.clone())].into());
    for host in [None, Some(explicit.clone())] {
        let expected = host.as_deref().unwrap_or(&env_host);
        let err = acquirer(&s, env.clone())
            .acquire(&request(host.clone()), &mut |_| {})
            .unwrap_err();
        assert_stopped(&err, expected);
        // The classification survives acquisition's finish boundary.
        assert!(matches!(err, EngineError::Io { .. }));
    }
    s.finish();
}

#[cfg(unix)]
#[test]
fn missing_default_socket_offers_the_other_socket_or_context_remedy() {
    let s = TestSandbox::for_test(TestCase::component("engine::oci::docker_engine::tests::missing_default_socket_offers_the_other_socket_or_context_remedy"));
    let path = s.ports().unix_socket("default.sock");
    let endpoint = Endpoint::DefaultUnix(path.clone());
    let err = ping(&endpoint);
    assert!(matches!(
        local_unavailability(&err),
        Some(LocalDockerUnavailable::Stopped {
            default_socket: true,
            ..
        })
    ));
    assert!(!retry::is_retryable(&err));
    let expected = format!("No Docker daemon at the default socket {}. Start Docker, or set the source's `host` (or DOCKER_HOST) if Docker uses another socket/context, then run `awman ready` again.", endpoint.describe());
    assert_eq!(failure_message(&err), expected);
    // A source-matched builder/import receives a materialized explicit host.
    let frozen_error = ping(&Endpoint::Unix(path));
    assert_eq!(failure_message_at(&frozen_error, Some(&endpoint)), expected);
    assert_eq!(
        endpoint.describe(),
        Endpoint::Unix(endpoint.unix_path().unwrap().into()).describe()
    );
    s.finish();
}

#[cfg(unix)]
#[test]
fn malformed_unix_socket_paths_are_final_configuration_diagnoses() {
    let s = TestSandbox::for_test(TestCase::component("engine::oci::docker_engine::tests::malformed_unix_socket_paths_are_final_configuration_diagnoses"));
    let file = s.work().join("regular-file");
    std::fs::write(&file, b"not a socket").unwrap();
    for path in [
        file.clone(),
        file.join("child.sock"),
        s.work().join("x".repeat(200)),
    ] {
        let endpoint = Endpoint::Unix(path);
        let error = ping(&endpoint);
        assert!(
            matches!(
                local_unavailability(&error),
                Some(LocalDockerUnavailable::InvalidSocket { .. })
            ),
            "{error}"
        );
        assert!(!retry::is_retryable(&error));
        let message = failure_message(&error);
        assert!(
            message.contains(&endpoint.describe())
                && message.contains("not a usable Docker socket"),
            "{message}"
        );
        assert!(!message.contains("Docker is not running") && !message.contains("io error at"));
    }
    s.finish();
}

#[cfg(unix)]
#[test]
/// hygiene-allow(H4): TestSandbox owns the socket path; the closed listener intentionally leaves a refusing socket, which the test removes.
fn refused_unix_socket_is_stopped_and_never_retried() {
    use std::os::unix::net::UnixListener;
    let s = TestSandbox::for_test(TestCase::component(
        "engine::oci::docker_engine::tests::refused_unix_socket_is_stopped_and_never_retried",
    ));
    let path = s.ports().unix_socket("refused.sock");
    drop(UnixListener::bind(&path).unwrap());
    let endpoint = Endpoint::Unix(path.clone());
    let err = ping(&endpoint);
    assert_stopped(&err, &endpoint.describe());
    std::fs::remove_file(path).unwrap();
    s.finish();
}

#[cfg(unix)]
#[test]
/// hygiene-allow(H4): TestSandbox owns the socket path; the test drops the listener and removes the socket before its leak check.
fn permission_denied_socket_keeps_permissions_remedy_and_is_final() {
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
    let s = TestSandbox::for_test(TestCase::component("engine::oci::docker_engine::tests::permission_denied_socket_keeps_permissions_remedy_and_is_final"));
    let path = s.ports().unix_socket("denied.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
    let endpoint = Endpoint::Unix(path.clone());
    // Root bypasses mode bits. Inject the same typed OS error in that
    // environment, without silently skipping the classification assertions.
    let err = match std::os::unix::net::UnixStream::connect(&path) {
        Err(e) => {
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
            ping(&endpoint)
        }
        Ok(_) => local_socket_failure(
            &endpoint,
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        )
        .unwrap(),
    };
    assert!(
        matches!(
            local_unavailability(&err),
            Some(LocalDockerUnavailable::PermissionDenied { .. })
        ),
        "{err}"
    );
    assert!(!retry::is_retryable(&err));
    let text = err.to_string();
    assert!(text.contains("Cannot access the Docker socket"));
    assert!(text.contains(&endpoint.describe()));
    assert!(text.contains("permissions") && text.contains("connection context"));
    assert!(!text.contains("Docker is not running"));
    drop(listener);
    std::fs::remove_file(path).unwrap();
    s.finish();
}

#[test]
fn unavailable_tcp_endpoint_is_retryable_and_never_claims_local_docker_stopped() {
    let s = TestSandbox::for_test(TestCase::component("engine::oci::docker_engine::tests::unavailable_tcp_endpoint_is_retryable_and_never_claims_local_docker_stopped"));
    let listener = s.ports().tcp_listener();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let endpoint = Endpoint::Tcp {
        host: addr.ip().to_string(),
        port: addr.port(),
        tls: None,
    };
    // Check the cause before Client formats it: Unix must exercise refusal;
    // Windows may time out during its slower loopback SYN retry sequence.
    let cancel = retry::CancelToken::new();
    let client = Client::new(
        &endpoint,
        OperationControl::new(cancel, retry::Deadline::start(POLICY.deadline)),
    )
    .unwrap();
    let raw = match client.transport.send(
        client
            .http
            .get(format!("{}/_ping", client.base))
            .timeout(Duration::from_secs(1)),
    ) {
        Err(super::super::transport::Error::Http(error)) => error,
        _ => panic!("fixture must produce a transport failure"),
    };
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&raw);
    let mut refused = false;
    while let Some(error) = cause {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            refused |= io.kind() == std::io::ErrorKind::ConnectionRefused;
            if let Some(inner) = io.get_ref() {
                cause = Some(inner);
                continue;
            }
        }
        cause = error.source();
    }
    assert!(refused || (cfg!(windows) && raw.is_timeout()), "{raw:?}");
    assert_eq!(ImageRetryClass::http(&raw), ImageRetryClass::Transient);
    assert!(local_socket_failure(&endpoint, &raw).is_none());
    let err = ping(&endpoint);
    assert!(retry::is_retryable(&err), "{err}");
    let text = err.to_string();
    assert!(
        text.contains(&endpoint.describe()) && text.contains("not reachable"),
        "{text}"
    );
    assert!(text.contains("after 3 of 3 attempts"), "{text}");
    assert!(!text.contains("Docker is not running"));
    assert!(local_unavailability(&err).is_none());
    s.finish();
}

#[test]
fn local_classification_uses_typed_causes_and_never_display_text_or_timeouts() {
    let endpoint = Endpoint::Unix("/private/custom.sock".into());
    let misleading = std::io::Error::other(
        "No such file or directory: Connection refused: Docker is not running",
    );
    assert!(local_socket_failure(&endpoint, &misleading).is_none());
    let nested = std::io::Error::other(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
    assert_stopped(
        &local_socket_failure(&endpoint, &nested).unwrap(),
        &endpoint.describe(),
    );
    let timeout = std::io::Error::from(std::io::ErrorKind::TimedOut);
    assert!(local_socket_failure(&endpoint, &timeout).is_none());
    let remote = Endpoint::Tcp {
        host: "remote.example.invalid".into(),
        port: 2376,
        tls: None,
    };
    assert!(local_socket_failure(&remote, &nested).is_none());
    #[cfg(unix)]
    {
        // macOS returns ENOTSOCK for a regular file, while Linux returns
        // ConnectionRefused. Exercise the OS error on both platforms.
        let not_socket = std::io::Error::other(std::io::Error::from_raw_os_error(libc::ENOTSOCK));
        let error = local_socket_failure(&endpoint, &not_socket)
            .expect("ENOTSOCK must be a final invalid-socket diagnosis");
        assert!(matches!(
            local_unavailability(&error),
            Some(LocalDockerUnavailable::InvalidSocket { .. })
        ));
        assert!(!retry::is_retryable(&error));
        assert!(local_socket_failure(&remote, &not_socket).is_none());
    }
    let err = resolve_endpoint(Some("ssh://host"), None, &EnvSnapshot::empty()).unwrap_err();
    assert!(matches!(err, EngineError::UnsupportedImageSource { .. }));
    assert!(!retry::is_retryable(&err));
    assert!(!err.to_string().contains("Docker is not running"));
}

/// One-request-per-connection fake daemon. Its owned listener, bounded reads,
/// wake-up and join keep failures from leaking processes, sockets or threads.
struct Daemon {
    addr: SocketAddr,
    cert: String,
    count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Daemon {
    fn start(s: &TestSandbox, tls: bool, archive: Option<Vec<u8>>) -> Self {
        let listener = s.ports().tcp_listener();
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
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let (done, accepts) = (stop.clone(), count.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else {
                    return;
                };
                accepts.fetch_add(1, Ordering::SeqCst);
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                if tls {
                    let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                    Self::serve(rustls::StreamOwned::new(conn, stream), archive.as_deref());
                } else {
                    Self::serve(stream, archive.as_deref());
                }
            }
        });
        Self {
            addr,
            cert,
            count,
            stop,
            thread: Some(thread),
        }
    }
    fn serve(mut stream: impl Read + Write, archive: Option<&[u8]>) {
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
            if stream.read(&mut byte).ok() != Some(1) {
                return;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let path = head.split_whitespace().nth(1).unwrap_or("");
        let inspect = serde_json::json!({ "Os": "linux", "Architecture": OciPlatform::host_linux().architecture }).to_string();
        let (status, body) = match (path, archive) {
            ("/_ping", _) => (200, b"OK".as_slice()),
            ("/version", _) => (200, br#"{"ApiVersion":"1.45"}"#.as_slice()),
            (p, Some(_)) if p.ends_with("/json") => (200, inspect.as_bytes()),
            (p, Some(archive)) if p.ends_with("/get") => (200, archive),
            _ => (404, b"image missing".as_slice()),
        };
        let header = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(header.as_bytes()).is_ok() {
            let _ = stream.write_all(body);
        }
    }
    fn host(&self) -> String {
        format!("tcp://{}", self.addr)
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn tls_certificate_failure_retains_its_cause_and_is_final() {
    let s = TestSandbox::for_test(TestCase::component(
        "engine::oci::docker_engine::tests::tls_certificate_failure_retains_its_cause_and_is_final",
    ));
    let daemon = Daemon::start(&s, true, None);
    let ca = s.work().join("ca.pem");
    std::fs::write(&ca, &daemon.cert).unwrap();
    let endpoint = Endpoint::Tcp {
        host: daemon.addr.ip().to_string(),
        port: daemon.addr.port(),
        tls: Some(DockerTlsConfig {
            ca,
            cert: None,
            key: None,
            verify: true,
        }),
    };
    let err = ping(&endpoint);
    assert!(!retry::is_retryable(&err), "{err}");
    assert!(local_unavailability(&err).is_none());
    let text = err.to_string();
    assert!(text.contains("certificate"), "{text}");
    assert!(!text.contains("Docker is not running"));
    assert_eq!(daemon.count.load(Ordering::SeqCst), 1);
    drop(daemon);
    s.finish();
}

#[test]
fn reachable_daemon_missing_image_is_final_and_not_stopped() {
    let s = TestSandbox::for_test(TestCase::component("engine::oci::docker_engine::tests::reachable_daemon_missing_image_is_final_and_not_stopped"));
    let daemon = Daemon::start(&s, false, None);
    let err = acquirer(&s, EnvSnapshot::empty())
        .acquire(&request(Some(daemon.host())), &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(err, EngineError::SourceImageNotFound { .. }),
        "{err}"
    );
    assert!(!retry::is_retryable(&err));
    assert!(err.to_string().contains("agent:latest") && err.to_string().contains(&daemon.host()));
    assert!(!err.to_string().contains("Docker is not running"));
    assert_eq!(daemon.count.load(Ordering::SeqCst), 3); // ping, version, inspect
    drop(daemon);
    s.finish();
}

#[test]
fn matching_cached_image_is_reused_after_daemon_stops() {
    let s = TestSandbox::for_test(TestCase::component(
        "engine::oci::docker_engine::tests::matching_cached_image_is_reused_after_daemon_stops",
    ));
    let path = s.work().join("image.tar");
    awman_test_support::fixtures::OciImageBuilder::new()
        .ref_name("agent:latest")
        .build()
        .write_archive(&path);
    let daemon = Daemon::start(&s, false, Some(std::fs::read(path).unwrap()));
    let request = request(Some(daemon.host()));
    let acquirer = acquirer(&s, EnvSnapshot::empty());
    let first = acquirer.acquire(&request, &mut |_| {}).unwrap();
    assert_eq!(daemon.count.load(Ordering::SeqCst), 4);
    let cached = acquirer.acquire(&request, &mut |_| {}).unwrap();
    assert_eq!(first.identity, cached.identity);
    assert_eq!(daemon.count.load(Ordering::SeqCst), 4);
    drop(daemon);
    let reused = acquirer.acquire(&request, &mut |_| {}).unwrap();
    assert_eq!(first.identity, reused.identity);
    drop((first, cached, reused));
    s.finish();
}
