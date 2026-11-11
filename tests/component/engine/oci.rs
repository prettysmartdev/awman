//! `engine::oci` — image acquisition through `CachingAcquirer` against
//! loopback doubles (`oci_doubles`): a Docker Engine on a Unix socket and
//! registry/Engine HTTP(S) servers on sandbox-owned TCP listeners.
//!
//! Re-derived from the socket-bound inline modules `oci::tests::engine_store::
//! fake_daemon` and `oci::retry_fixtures`. Retry backoff is never slept
//! (`with_sleeper`), and attempt counts are read from the servers.

use super::oci_doubles::{acquirer, agent_archive, arm64, cache_entries, hex, HttpFixture, Reply};
use crate::support;
use awman::data::config::env::EnvSnapshot;
use awman::data::config::image_source::{
    DockerTlsConfig, ImageSourceKind, ImageSourceSpec, RegistryHostConfig,
};
use awman::engine::error::EngineError;
use awman::engine::oci::{AcquirePolicy, AcquireRequest, ArchiveFormat, ImageAcquirer};
use awman_test_support::TestSandbox;
use std::collections::BTreeMap;
use std::path::Path;

const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

fn state(sandbox: &TestSandbox) -> std::path::PathBuf {
    let dir = sandbox.work().join("state");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ─── Docker Engine store over a Unix socket ─────────────────────────────────

#[cfg(unix)]
fn engine_request(socket: &Path, policy: AcquirePolicy) -> AcquireRequest {
    AcquireRequest {
        tag: "awman-x-claude:latest".into(),
        source: ImageSourceSpec::DockerStore {
            host: Some(format!("unix://{}", socket.display())),
            tls: None,
            reference: None,
        },
        platform: arm64(),
        policy,
        registries: BTreeMap::new(),
    }
}

#[cfg(unix)]
fn engine_acquire(
    state: &Path,
    request: &AcquireRequest,
) -> Result<awman::engine::oci::LeasedImage, EngineError> {
    acquirer(state, EnvSnapshot::empty()).acquire(request, &mut |_| {})
}

#[cfg_attr(not(unix), ignore = "not-applicable: Engine double needs Unix sockets")]
#[test]
fn engine_export_is_validated_cached_and_reused_without_the_daemon() {
    let sandbox = support::sandbox(
        "engine::oci::engine_export_is_validated_cached_and_reused_without_the_daemon",
    );
    #[cfg(unix)]
    {
        use super::oci_doubles::{standard_engine, EngineDouble};
        let state = state(&sandbox);
        let engine = EngineDouble::start(
            &sandbox,
            "d.sock",
            standard_engine("1.45", ("linux", "arm64"), agent_archive(), false),
        );
        let socket = engine.socket.clone();
        let got =
            engine_acquire(&state, &engine_request(&socket, AcquirePolicy::IfMissing)).unwrap();
        assert_eq!(got.identity.source, ImageSourceKind::DockerStore);
        assert_eq!(got.archive_format, ArchiveFormat::DockerSave);
        assert_eq!(got.identity.reference, "awman-x-claude:latest");
        assert!(engine
            .paths()
            .contains(&"/v1.41/images/awman-x-claude:latest/get".to_string()));
        // The daemon disappears; running the cached image does not care.
        engine.stop();
        let cached =
            engine_acquire(&state, &engine_request(&socket, AcquirePolicy::CachedOnly)).unwrap();
        assert_eq!(cached, got);
    }
    sandbox.finish();
}

#[cfg_attr(not(unix), ignore = "not-applicable: Engine double needs Unix sockets")]
#[test]
fn a_legacy_docker_cache_key_is_found_without_contacting_the_source() {
    let sandbox = support::sandbox(
        "engine::oci::a_legacy_docker_cache_key_is_found_without_contacting_the_source",
    );
    #[cfg(unix)]
    {
        use super::oci_doubles::{standard_engine, EngineDouble};
        use awman::data::oci_source_key::OciSourceKey;
        let state = state(&sandbox);
        let engine = EngineDouble::start(
            &sandbox,
            "d.sock",
            standard_engine("1.45", ("linux", "arm64"), agent_archive(), false),
        );
        let socket = engine.socket.clone();
        let first =
            engine_acquire(&state, &engine_request(&socket, AcquirePolicy::IfMissing)).unwrap();
        let count_before = engine.paths().len();
        let key = OciSourceKey {
            kind: ImageSourceKind::DockerStore,
            locator: format!("unix://{}", socket.display()),
            reference: "awman-x-claude:latest".into(),
            platform: arm64(),
        };
        let refs = state.join("oci-cache/refs");
        let canonical = refs.join(format!("{}.json", hex(key.canonical_key().as_bytes())));
        let legacy_key = format!(
            "v1\ndocker-store\n{}\n{}\n{}",
            key.locator, key.reference, key.platform
        );
        let legacy = refs.join(format!("{}.json", hex(legacy_key.as_bytes())));
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&canonical).unwrap()).unwrap();
        record["key"] = legacy_key.into();
        std::fs::write(&canonical, serde_json::to_vec(&record).unwrap()).unwrap();
        std::fs::rename(canonical, legacy).unwrap();
        let hit =
            engine_acquire(&state, &engine_request(&socket, AcquirePolicy::IfMissing)).unwrap();
        assert_eq!(hit.identity, first.identity);
        assert_eq!(engine.paths().len(), count_before);
    }
    sandbox.finish();
}

#[cfg_attr(not(unix), ignore = "not-applicable: Engine double needs Unix sockets")]
#[test]
fn new_engines_export_the_native_platform_even_when_the_default_variant_differs() {
    let sandbox = support::sandbox(
        "engine::oci::new_engines_export_the_native_platform_even_when_the_default_variant_differs",
    );
    #[cfg(unix)]
    {
        use super::oci_doubles::{standard_engine, EngineDouble};
        for (name, default_arch) in [("a.sock", "arm64"), ("b.sock", "amd64")] {
            let state = sandbox.work().join(name);
            std::fs::create_dir_all(&state).unwrap();
            let engine = EngineDouble::start(
                &sandbox,
                name,
                standard_engine("1.49", ("linux", default_arch), agent_archive(), false),
            );
            engine_acquire(
                &state,
                &engine_request(&engine.socket, AcquirePolicy::IfMissing),
            )
            .unwrap();
            let paths = engine.paths();
            let get = paths.iter().find(|p| p.contains("/get")).unwrap();
            assert!(
                get.starts_with("/v1.48/images/awman-x-claude:latest/get?platform="),
                "{get}"
            );
            assert!(get.contains("arm64"), "{get}");
        }
    }
    sandbox.finish();
}

#[cfg_attr(not(unix), ignore = "not-applicable: Engine double needs Unix sockets")]
#[test]
fn an_old_engine_with_the_wrong_platform_is_refused_before_export() {
    let sandbox = support::sandbox(
        "engine::oci::an_old_engine_with_the_wrong_platform_is_refused_before_export",
    );
    #[cfg(unix)]
    {
        use super::oci_doubles::{standard_engine, EngineDouble};
        let state = state(&sandbox);
        let engine = EngineDouble::start(
            &sandbox,
            "d.sock",
            standard_engine("1.45", ("linux", "amd64"), agent_archive(), false),
        );
        let err = engine_acquire(
            &state,
            &engine_request(&engine.socket, AcquirePolicy::IfMissing),
        )
        .unwrap_err();
        assert!(
            matches!(err, EngineError::ImagePlatformMismatch { .. }),
            "{err:?}"
        );
        assert!(!engine.paths().iter().any(|p| p.contains("/get")));
    }
    sandbox.finish();
}

#[cfg_attr(not(unix), ignore = "not-applicable: Engine double needs Unix sockets")]
#[test]
fn a_missing_image_names_the_engine_endpoint() {
    let sandbox = support::sandbox("engine::oci::a_missing_image_names_the_engine_endpoint");
    #[cfg(unix)]
    {
        use super::oci_doubles::{standard_engine, EngineDouble};
        let state = state(&sandbox);
        let engine = EngineDouble::start(
            &sandbox,
            "d.sock",
            standard_engine("1.45", ("linux", "arm64"), agent_archive(), false),
        );
        let mut request = engine_request(&engine.socket, AcquirePolicy::IfMissing);
        request.source = ImageSourceSpec::DockerStore {
            host: Some(format!("unix://{}", engine.socket.display())),
            tls: None,
            reference: Some("nope:1".into()),
        };
        let text = engine_acquire(&state, &request).unwrap_err().to_string();
        assert!(
            text.contains("nope:1") && text.contains("unix://"),
            "{text}"
        );
    }
    sandbox.finish();
}

#[cfg_attr(not(unix), ignore = "not-applicable: Engine double needs Unix sockets")]
#[test]
fn a_truncated_export_leaves_no_cache_state() {
    let sandbox = support::sandbox("engine::oci::a_truncated_export_leaves_no_cache_state");
    #[cfg(unix)]
    {
        use super::oci_doubles::{standard_engine, EngineDouble};
        let state = state(&sandbox);
        let engine = EngineDouble::start(
            &sandbox,
            "d.sock",
            standard_engine("1.45", ("linux", "arm64"), agent_archive(), true),
        );
        let err = engine_acquire(
            &state,
            &engine_request(&engine.socket, AcquirePolicy::IfMissing),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                EngineError::Network(_) | EngineError::ImageArchiveRejected { .. }
            ),
            "{err:?}"
        );
        assert_eq!(cache_entries(&state, "images"), 0);
        assert_eq!(cache_entries(&state, "tmp"), 0);
    }
    sandbox.finish();
}

#[cfg_attr(not(unix), ignore = "not-applicable: Engine double needs Unix sockets")]
#[test]
fn an_unreachable_engine_is_a_network_error() {
    let sandbox = support::sandbox("engine::oci::an_unreachable_engine_is_a_network_error");
    #[cfg(unix)]
    {
        let state = state(&sandbox);
        // A socket path the sandbox owns but nothing listens on.
        let socket = sandbox.ports().unix_socket("absent.sock");
        let err =
            engine_acquire(&state, &engine_request(&socket, AcquirePolicy::IfMissing)).unwrap_err();
        assert!(
            matches!(err, EngineError::Network(ref m) if m.contains("not reachable")),
            "{err:?}"
        );
    }
    sandbox.finish();
}

// ─── Registry and Engine HTTP(S): classification and attempt counts ─────────

fn registry_request(server: &HttpFixture, reference: &str, tls: bool) -> AcquireRequest {
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

/// Acquire, expecting failure, and check nothing was staged or cached.
fn acquire_fails(state: &Path, request: &AcquireRequest) -> EngineError {
    std::fs::create_dir_all(state).unwrap();
    let err = acquirer(state, EnvSnapshot::empty())
        .acquire(request, &mut |_| {})
        .unwrap_err();
    assert_eq!(cache_entries(state, "tmp"), 0, "staging left: {err}");
    assert_eq!(cache_entries(state, "images"), 0, "image cached: {err}");
    err
}

fn manifest(config: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": MANIFEST,
        "config": {"digest": format!("sha256:{}", hex(config)), "size": config.len()},
        "layers": [],
    }))
    .unwrap()
}

#[test]
fn malformed_registry_json_schema_and_digest_fields_are_final() {
    let sandbox =
        support::sandbox("engine::oci::malformed_registry_json_schema_and_digest_fields_are_final");
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
        let server = HttpFixture::start(&sandbox, false, move |path| {
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
        let state = sandbox.work().join(case);
        let err = acquire_fails(
            &state,
            &registry_request(&server, "team/tls-agent:1", false),
        );
        assert!(matches!(err, EngineError::Network(_)), "{case}: {err}");
        assert_eq!(
            server.count("/v2/team/tls-agent/manifests/1"),
            1,
            "{case}: {err}"
        );
    }
    sandbox.finish();
}

#[test]
fn registry_digest_and_platform_mismatches_are_final() {
    let sandbox =
        support::sandbox("engine::oci::registry_digest_and_platform_mismatches_are_final");
    for platform in [false, true] {
        let server = HttpFixture::start(&sandbox, false, move |_| {
            if platform {
                Reply::body(br#"{"manifests":[{"digest":"unused","size":1,"platform":{"os":"linux","architecture":"s390x"}}]}"#.to_vec(), INDEX)
            } else {
                // A digest-pinned reference cannot resolve to these bytes.
                Reply::body("{}", MANIFEST)
            }
        });
        let reference = if platform {
            "team/agent:1".to_string()
        } else {
            format!("team/agent@sha256:{}", "f".repeat(64))
        };
        let state = sandbox.work().join(format!("platform-{platform}"));
        let err = acquire_fails(&state, &registry_request(&server, &reference, false));
        assert!(
            matches!(
                err,
                EngineError::ImagePlatformMismatch { .. } | EngineError::ImageDigestMismatch { .. }
            ),
            "{err}"
        );
        assert_eq!(server.accepted(), 1, "{err}");
    }
    sandbox.finish();
}

#[test]
fn only_5xx_and_truncation_are_retried_whatever_the_reference_says() {
    let sandbox = support::sandbox(
        "engine::oci::only_5xx_and_truncation_are_retried_whatever_the_reference_says",
    );
    let mut n = 0;
    for reference in ["team/agent:1", "team/tls-agent:1"] {
        for status in [400u16, 401, 403, 404, 500, 503] {
            let server = HttpFixture::start(&sandbox, false, move |_| Reply::status(status));
            n += 1;
            let state = sandbox.work().join(format!("s{n}"));
            let err = acquire_fails(&state, &registry_request(&server, reference, false));
            assert_eq!(
                server.accepted(),
                if status >= 500 { 3 } else { 1 },
                "{status}: {err}"
            );
            if [401, 403].contains(&status) {
                assert!(matches!(err, EngineError::Auth(_)), "{status}: {err}");
            }
        }
        for blob in [false, true] {
            let server = HttpFixture::start(&sandbox, false, move |path| {
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
            n += 1;
            let state = sandbox.work().join(format!("s{n}"));
            let err = acquire_fails(&state, &registry_request(&server, reference, false));
            let repository = reference.trim_end_matches(":1");
            assert_eq!(
                server.count(&format!("/v2/{repository}/manifests/1")),
                3,
                "{err}"
            );
        }
    }
    sandbox.finish();
}

#[test]
fn docker_engine_metadata_decoding_and_status_have_typed_attempt_counts() {
    let sandbox = support::sandbox(
        "engine::oci::docker_engine_metadata_decoding_and_status_have_typed_attempt_counts",
    );
    let mut n = 0;
    for at_inspect in [false, true] {
        for (status, body, attempts) in [(200, "{", 1), (200, "{}", 1), (403, "", 1), (503, "", 3)]
        {
            let server = HttpFixture::start(&sandbox, false, move |path| {
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
            let mut request = registry_request(&server, "team/tls-agent:1", false);
            request.source = ImageSourceSpec::DockerStore {
                host: Some(format!("tcp://{}", server.addr)),
                tls: None,
                reference: Some("tls-agent:latest".into()),
            };
            n += 1;
            let state = sandbox.work().join(format!("s{n}"));
            let err = acquire_fails(&state, &request);
            assert_eq!(
                server.count("/_ping"),
                attempts,
                "inspect={at_inspect} {status} {body}: {err}"
            );
        }
    }
    sandbox.finish();
}

#[test]
fn tls_refusals_stop_after_one_connection_for_registry_and_engine() {
    let sandbox = support::sandbox(
        "engine::oci::tls_refusals_stop_after_one_connection_for_registry_and_engine",
    );
    for docker in [false, true] {
        let server = HttpFixture::start(&sandbox, true, |_| Reply::status(599));
        let state = sandbox.work().join(format!("docker-{docker}"));
        std::fs::create_dir_all(&state).unwrap();
        let mut request = registry_request(&server, "team/tls-agent:1", true);
        if docker {
            let ca = state.join("ca.pem");
            std::fs::write(&ca, &server.cert).unwrap();
            request.source = ImageSourceSpec::DockerStore {
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
        // Registry rejects the unknown issuer; Docker trusts the issuer but
        // rejects the certificate's `localhost` name for 127.0.0.1.
        let err = acquire_fails(&state, &request);
        assert!(matches!(err, EngineError::Network(_)), "{err}");
        assert_eq!(server.accepted(), 1, "docker={docker}: {err}");
    }
    sandbox.finish();
}

#[test]
fn a_proxy_authentication_refusal_is_final() {
    let sandbox = support::sandbox("engine::oci::a_proxy_authentication_refusal_is_final");
    let proxy = HttpFixture::start(&sandbox, false, |_| Reply::status(407));
    let proxy_url = format!("http://{}", proxy.addr);
    let env = EnvSnapshot::with_overrides([
        ("HTTPS_PROXY", proxy_url.as_str()),
        ("https_proxy", proxy_url.as_str()),
    ]);
    let request = AcquireRequest {
        tag: "awman-test:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some("registry.example.invalid".into()),
            reference: Some("team/agent:1".into()),
        },
        platform: arm64(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::new(),
    };
    let state = state(&sandbox);
    let err = acquirer(&state, env)
        .acquire(&request, &mut |_| {})
        .unwrap_err();
    assert_eq!(proxy.accepted(), 1, "a 407 is not retried: {err}");
    drop(proxy);
    sandbox.finish();
}
