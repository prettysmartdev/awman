//! Docker Engine source through disposable loopback daemons: Unix socket
//! and TLS endpoints, older/newer API versions, missing images, disconnects,
//! truncation, bounded retry, cancellation and certificate failures.
//!
//! The daemons are doubles built in this process (see `support`). They are
//! not a Docker Engine; the real-service tier lives in `real_stores.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use awman::data::config::env::EnvSnapshot;
use awman::data::config::image_source::{DockerTlsConfig, ImageSourceKind, ImageSourceSpec};
use awman::data::oci_identity::OciPlatform;
use awman::engine::error::EngineError;
use awman::engine::oci::retry::is_cancelled;
use awman::engine::oci::{
    AcquirePolicy, AcquireRequest, ArchiveFormat, CancelToken, ImageAcquirer, RetryPolicy,
};

use crate::support::*;

const REFERENCE: &str = "awman-x-claude:latest";

fn fixture_archive() -> Vec<u8> {
    docker_save(&[Image::new(
        OciPlatform::host_linux(),
        realistic_layers(),
        REFERENCE,
    )])
}

fn host_os_arch() -> (&'static str, &'static str) {
    let p = OciPlatform::host_linux();
    let arch: &'static str = match p.architecture.as_str() {
        "arm64" => "arm64",
        "amd64" => "amd64",
        _ => "unknown",
    };
    ("linux", arch)
}

fn request(host: String, tls: Option<DockerTlsConfig>, policy: AcquirePolicy) -> AcquireRequest {
    AcquireRequest {
        tag: REFERENCE.into(),
        source: ImageSourceSpec::DockerStore {
            host: Some(host),
            tls,
            reference: None,
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: Default::default(),
    }
}

// ── Unix socket ─────────────────────────────────────────────────────────────

#[test]
fn store_docker_unix_roundtrip_then_cached_use_after_the_daemon_is_gone() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let engine = FakeEngine::start(
        temp.path(),
        engine_script(
            "1.45",
            host_os_arch(),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::ok(a),
        ),
    );
    let acq = acquirer(&state);
    let got = acq
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(got.identity.source, ImageSourceKind::DockerStore);
    assert_eq!(got.archive_format, ArchiveFormat::DockerSave);
    assert_eq!(engine.exports(), 1);

    engine.stop();
    let cached = acq
        .acquire(
            &request(engine.host(), None, AcquirePolicy::CachedOnly),
            &mut |_| {},
        )
        .expect("cached use needs no daemon");
    assert_eq!(cached, got);
    // Refreshing with the daemon gone is a clean network error, not a cache loss.
    let err = acq
        .acquire(
            &request(engine.host(), None, AcquirePolicy::Refresh),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Network(_)), "{err:?}");
    assert!(acq
        .acquire(
            &request(engine.host(), None, AcquirePolicy::CachedOnly),
            &mut |_| {},
        )
        .is_ok());
}

#[test]
fn store_docker_old_supported_api_exports_without_platform_and_older_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        temp.path(),
        engine_script(
            "1.41",
            host_os_arch(),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::ok(a),
        ),
    );
    acquirer(&temp.path().join("a"))
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    let export = engine
        .requests()
        .into_iter()
        .find(|p| p.contains("/get"))
        .unwrap();
    assert!(export.starts_with("/v1.41/"), "{export}");
    assert!(!export.contains("platform="), "{export}");

    let old = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        old.path(),
        engine_script(
            "1.40",
            host_os_arch(),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::ok(a),
        ),
    );
    let err = acquirer(&old.path().join("a"))
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::UnsupportedImageSource { ref reason, .. } if reason.contains("1.41")),
        "{err:?}"
    );
    assert_eq!(
        engine.exports(),
        0,
        "an unsupported engine is never asked to export"
    );
    assert_eq!(
        engine
            .requests()
            .iter()
            .filter(|p| *p == "/version")
            .count(),
        1,
        "unsupported API is final: no retry"
    );
}

#[test]
fn store_docker_multi_platform_store_exports_the_requested_platform() {
    // The unqualified inspect reports the store's default (foreign) variant;
    // a 1.48+ engine is asked for the host platform explicitly and the
    // exported config decides.
    let temp = tempfile::tempdir().unwrap();
    let foreign = other_platform(&OciPlatform::host_linux());
    let foreign_arch: &'static str = if foreign.architecture == "amd64" {
        "amd64"
    } else {
        "arm64"
    };
    let engine = FakeEngine::start(
        temp.path(),
        engine_script(
            "1.49",
            ("linux", foreign_arch),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::ok(a),
        ),
    );
    let got = acquirer(&temp.path().join("a"))
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(got.identity.platform, OciPlatform::host_linux());
    let export = engine
        .requests()
        .into_iter()
        .find(|p| p.contains("/get"))
        .unwrap();
    assert!(export.starts_with("/v1.48/"), "{export}");
    assert!(
        export.contains(&OciPlatform::host_linux().architecture),
        "{export}"
    );

    // An old engine that can only export its default variant is refused
    // before any export.
    let old = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        old.path(),
        engine_script(
            "1.45",
            ("linux", foreign_arch),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::ok(a),
        ),
    );
    let err = acquirer(&old.path().join("a"))
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::ImagePlatformMismatch { .. }),
        "{err:?}"
    );
    assert_eq!(engine.exports(), 0);
}

#[test]
fn store_docker_missing_image_names_the_endpoint_and_is_not_retried() {
    let temp = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        temp.path(),
        engine_script(
            "1.45",
            host_os_arch(),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::ok(a),
        ),
    );
    let mut req = request(engine.host(), None, AcquirePolicy::IfMissing);
    req.source = ImageSourceSpec::DockerStore {
        host: Some(engine.host()),
        tls: None,
        reference: Some("missing/agent:9".into()),
    };
    let err = acquirer(&temp.path().join("a"))
        .acquire(&req, &mut |_| {})
        .unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, EngineError::Container(_)), "{err:?}");
    assert!(
        text.contains("missing/agent:9") && text.contains("unix://"),
        "{text}"
    );
    assert_eq!(
        engine
            .requests()
            .iter()
            .filter(|p| p.contains("/images/missing/agent:9/json"))
            .count(),
        1,
        "a missing image is final"
    );
    assert_eq!(cached_archives(&temp.path().join("a")), 0);
}

#[test]
fn acquire_retry_is_bounded_and_source_stable() {
    // The first two exports disconnect mid-transfer; the third succeeds.
    let temp = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        temp.path(),
        engine_script("1.45", host_os_arch(), REFERENCE, fixture_archive(), {
            let exports = AtomicUsize::new(0);
            move |_, a| {
                let n = exports.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Reply::truncated(a, 700)
                } else {
                    Reply::ok(a)
                }
            }
        }),
    );
    let state = temp.path().join("a");
    let sleeps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = sleeps.clone();
    let acq = acquirer(&state)
        .with_retry(RetryPolicy {
            max_attempts: 3,
            deadline: Duration::from_secs(60),
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
        })
        .with_sleeper(Arc::new(move |d| log.lock().unwrap().push(d)));
    let got = acq
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .expect("third attempt succeeds");
    assert_eq!(got.identity.reference, REFERENCE);
    assert_eq!(engine.exports(), 3, "exactly one export per attempt");
    assert_eq!(
        *sleeps.lock().unwrap(),
        vec![Duration::from_millis(5), Duration::from_millis(10)],
        "exponential backoff between attempts"
    );
    // Every request went to the same socket: the endpoint never changed.
    assert!(engine
        .requests()
        .iter()
        .all(|p| p.starts_with("/v1.41/") || p == "/_ping" || p == "/version"));
    assert_eq!(cached_archives(&state), 1);
    assert_eq!(staging_dirs(&state), 0, "failed attempts leave no staging");

    // With fewer attempts than failures the acquisition fails, saying so.
    std::fs::create_dir_all(temp.path().join("two")).unwrap();
    let engine2 = FakeEngine::start(
        &temp.path().join("two"),
        engine_script(
            "1.45",
            host_os_arch(),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::truncated(a, 700),
        ),
    );
    let state2 = temp.path().join("b");
    let err = acquirer(&state2)
        .with_retry(RetryPolicy {
            max_attempts: 2,
            ..RetryPolicy::DEFAULT
        })
        .with_sleeper(Arc::new(|_| {}))
        .acquire(
            &request(engine2.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, EngineError::Network(_)), "{err:?}");
    assert!(text.contains("after 2 of 2 attempts"), "{text}");
    assert!(
        text.contains("truncated") || text.contains("disconnected"),
        "{text}"
    );
    assert_eq!(engine2.exports(), 2);
    assert_eq!(cached_archives(&state2), 0);
    assert_eq!(staging_dirs(&state2), 0);
}

#[test]
fn a_daemon_that_drops_the_connection_is_retried_and_5xx_is_transient() {
    let temp = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        temp.path(),
        engine_script("1.45", host_os_arch(), REFERENCE, fixture_archive(), {
            let exports = AtomicUsize::new(0);
            move |_, a| match exports.fetch_add(1, Ordering::SeqCst) {
                0 => Reply::dropped(),
                1 => Reply::status(500, b"{\"message\":\"daemon busy\"}"),
                _ => Reply::ok(a),
            }
        }),
    );
    let got = acquirer(&temp.path().join("a"))
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(got.identity.reference, REFERENCE);
    assert_eq!(engine.exports(), 3);
}

#[test]
fn acquire_cancel_removes_partial_transfer() {
    let temp = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        temp.path(),
        engine_script(
            "1.45",
            host_os_arch(),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::stalled(a, 1024, Duration::from_secs(5)),
        ),
    );
    let state = temp.path().join("a");
    let token = CancelToken::new();
    let acq = acquirer(&state).with_cancel(token.clone());
    let canceller = {
        let token = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            token.cancel();
        })
    };
    let started = std::time::Instant::now();
    let err = acq
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    canceller.join().unwrap();
    assert!(is_cancelled(&err), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "cancellation interrupted the stalled transfer"
    );
    assert_eq!(
        engine.exports(),
        1,
        "a cancelled acquisition is not retried"
    );
    assert_eq!(cached_archives(&state), 0);
    assert_eq!(staging_dirs(&state), 0, "the partial transfer is gone");

    // A cancelled token also stops a new acquisition before it contacts
    // anything.
    let err = acq
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(is_cancelled(&err));
    assert_eq!(engine.exports(), 1);
}

#[test]
fn the_acquisition_deadline_bounds_a_stalled_export() {
    let temp = tempfile::tempdir().unwrap();
    let engine = FakeEngine::start(
        temp.path(),
        engine_script(
            "1.45",
            host_os_arch(),
            REFERENCE,
            fixture_archive(),
            |_, a| Reply::stalled(a, 1024, Duration::from_secs(5)),
        ),
    );
    let state = temp.path().join("a");
    let err = acquirer(&state)
        .with_retry(RetryPolicy {
            max_attempts: 3,
            deadline: Duration::from_millis(800),
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
        })
        .acquire(
            &request(engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Network(_)), "{err:?}");
    assert!(engine.exports() <= 2, "no retries past the deadline");
    assert_eq!(cached_archives(&state), 0);
}

// ── TLS ─────────────────────────────────────────────────────────────────────

fn tls(material: &TlsMaterial, ca: &std::path::Path, mtls: bool, verify: bool) -> DockerTlsConfig {
    DockerTlsConfig {
        ca: ca.to_path_buf(),
        cert: mtls.then(|| material.client_cert_pem.clone()),
        key: mtls.then(|| material.client_key_pem.clone()),
        verify,
    }
}

#[test]
fn store_docker_tls_roundtrip_with_a_private_ca() {
    let material = tls_material(&["localhost"], false);
    let service = serve_https(
        engine_app("1.45", host_os_arch(), REFERENCE, fixture_archive()),
        &material,
    );
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("a");
    let host = format!("tcp://127.0.0.1:{}", service.addr.port());
    let acq = acquirer(&state);
    let got = acq
        .acquire(
            &request(
                host.clone(),
                Some(tls(&material, &material.ca_pem, false, true)),
                AcquirePolicy::IfMissing,
            ),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(got.identity.source, ImageSourceKind::DockerStore);
    assert!(service
        .requests()
        .iter()
        .any(|(p, _)| p.contains("/images/awman-x-claude:latest/get")));

    service.stop();
    let cached = acq
        .acquire(
            &request(
                host.clone(),
                Some(tls(&material, &material.ca_pem, false, true)),
                AcquirePolicy::CachedOnly,
            ),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(cached, got);
}

#[test]
fn store_docker_tls_wrong_ca_expired_and_mismatched_certificates_are_final() {
    let temp = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, TlsMaterial, bool)> = vec![
        ("wrong CA", tls_material(&["localhost"], false), true),
        ("expired", tls_material(&["localhost"], true), false),
        (
            "name mismatch",
            tls_material(&["other.example"], false),
            false,
        ),
    ];
    for (i, (name, material, other_ca)) in cases.into_iter().enumerate() {
        let service = serve_https(
            engine_app("1.45", host_os_arch(), REFERENCE, fixture_archive()),
            &material,
        );
        let state = temp.path().join(format!("s{i}"));
        let host = if name == "name mismatch" {
            format!("tcp://localhost:{}", service.addr.port())
        } else {
            format!("tcp://127.0.0.1:{}", service.addr.port())
        };
        let ca = if other_ca {
            &material.other_ca_pem
        } else {
            &material.ca_pem
        };
        let err = acquirer(&state)
            .acquire(
                &request(
                    host,
                    Some(tls(&material, ca, false, true)),
                    AcquirePolicy::IfMissing,
                ),
                &mut |_| {},
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Network(_)), "{name}: {err:?}");
        let exported = service.requests().iter().any(|(p, _)| p.contains("/get"));
        assert!(
            !exported,
            "{name}: nothing is exported over an untrusted channel"
        );
        assert_eq!(cached_archives(&state), 0, "{name}");
        // Certificate failures are final: one connection attempt, no retry.
        let pings = service
            .requests()
            .iter()
            .filter(|(p, _)| p == "/_ping")
            .count();
        assert_eq!(pings, 0, "{name}: the handshake never completed");
        assert!(
            !err.to_string().contains("attempts against the same source"),
            "{name}: no retries were made: {err}"
        );
    }
}

#[test]
fn store_docker_tls_verify_false_accepts_an_unknown_certificate() {
    let material = tls_material(&["localhost"], false);
    let service = serve_https(
        engine_app("1.45", host_os_arch(), REFERENCE, fixture_archive()),
        &material,
    );
    let temp = tempfile::tempdir().unwrap();
    let host = format!("tcp://127.0.0.1:{}", service.addr.port());
    let got = acquirer(&temp.path().join("a"))
        .acquire(
            &request(
                host,
                Some(tls(&material, &material.other_ca_pem, false, false)),
                AcquirePolicy::IfMissing,
            ),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(got.identity.reference, REFERENCE);
}

#[test]
fn store_docker_tls_client_certificate_material_is_read_and_validated() {
    // Mutual TLS: the client identity is built from the configured PEM
    // pair. Without a `rustls` dev-dependency the loopback server cannot
    // *require* the certificate (REQ-acquisition-001); this proves the
    // identity loads, is presented on a TLS session that accepts it, and
    // that a half-configured pair is refused before any connection.
    let material = tls_material(&["localhost"], false);
    let service = serve_https(
        engine_app("1.45", host_os_arch(), REFERENCE, fixture_archive()),
        &material,
    );
    let temp = tempfile::tempdir().unwrap();
    let host = format!("tcp://127.0.0.1:{}", service.addr.port());
    let got = acquirer(&temp.path().join("a"))
        .acquire(
            &request(
                host.clone(),
                Some(tls(&material, &material.ca_pem, true, true)),
                AcquirePolicy::IfMissing,
            ),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(got.identity.reference, REFERENCE);

    let half = DockerTlsConfig {
        ca: material.ca_pem.clone(),
        cert: Some(material.client_cert_pem.clone()),
        key: None,
        verify: true,
    };
    let before = service.requests().len();
    let err = acquirer(&temp.path().join("b"))
        .acquire(
            &request(host.clone(), Some(half), AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Config(ref m) if m.contains("both `cert` and `key`")),
        "{err:?}"
    );
    assert_eq!(
        service.requests().len(),
        before,
        "refused before connecting"
    );

    let bad_key = DockerTlsConfig {
        ca: material.ca_pem.clone(),
        cert: Some(material.client_cert_pem.clone()),
        key: Some(material.ca_pem.clone()),
        verify: true,
    };
    let err = acquirer(&temp.path().join("c"))
        .acquire(
            &request(host, Some(bad_key), AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Config(_)), "{err:?}");
}

#[test]
fn docker_tls_defaults_come_from_the_injected_environment_not_the_real_home() {
    // DOCKER_HOST/DOCKER_TLS_VERIFY/DOCKER_CERT_PATH are taken from the
    // snapshot the acquirer was given; nothing reads ~/.docker here.
    let material = tls_material(&["localhost"], false);
    let service = serve_https(
        engine_app("1.45", host_os_arch(), REFERENCE, fixture_archive()),
        &material,
    );
    let certs = material.dir.path().join("certs");
    std::fs::create_dir(&certs).unwrap();
    std::fs::copy(&material.ca_pem, certs.join("ca.pem")).unwrap();
    std::fs::copy(&material.client_cert_pem, certs.join("cert.pem")).unwrap();
    std::fs::copy(&material.client_key_pem, certs.join("key.pem")).unwrap();
    let env = EnvSnapshot::with_overrides([
        (
            "DOCKER_HOST",
            format!("tcp://127.0.0.1:{}", service.addr.port()),
        ),
        ("DOCKER_TLS_VERIFY", "1".to_string()),
        ("DOCKER_CERT_PATH", certs.display().to_string()),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let req = AcquireRequest {
        tag: REFERENCE.into(),
        source: ImageSourceSpec::DockerStore {
            host: None,
            tls: None,
            reference: None,
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: Default::default(),
    };
    let got = acquirer_with_env(&temp.path().join("a"), env)
        .acquire(&req, &mut |_| {})
        .unwrap();
    assert_eq!(got.identity.reference, REFERENCE);
}
