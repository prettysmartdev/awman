//! Registry source through disposable loopback services: private CA over
//! TLS, Bearer challenges with same-origin and foreign token realms, native
//! (env / in-memory keychain) credentials, HTTP CONNECT proxy with
//! `NO_PROXY` bypass, proxy credentials, and secret redaction under failure.
//!
//! Every server is built in this process; nothing contacts a real registry
//! or credential store. Native keychain entries are exercised only by the
//! separately gated `real_stores.rs`.

use std::collections::BTreeMap;

use awman::data::config::env::EnvSnapshot;
use awman::data::config::image_source::{
    ImageSourceKind, ImageSourceSpec, RegistryAuthSource, RegistryHostConfig,
};
use awman::data::oci_identity::OciPlatform;
use awman::engine::error::EngineError;
use awman::engine::oci::{AcquirePolicy, AcquireRequest, ArchiveFormat, ImageAcquirer};

use crate::support::*;

const REPO: &str = "team/agent";
const PASSWORD: &str = "SENTINEL-registry-password-7f3a";
const TOKEN: &str = "SENTINEL-bearer-token-value-91c2";
const PROXY_PASSWORD: &str = "SENTINEL-proxy-password-55e1";

fn image() -> Image {
    Image::new(
        OciPlatform::host_linux(),
        realistic_layers(),
        "team/agent:1",
    )
}

/// A registry over TLS whose token realm is on its own origin. The realm
/// URL must be known before the server starts, so a free port is picked
/// first. Returns the service, the TLS material (keep it alive: the CA file
/// lives in its directory) and the served image.
fn registry(with_auth: bool) -> (Service, TlsMaterial, RegistryImage) {
    let material = tls_material(&["localhost"], false);
    let img = registry_image(&image());
    let port = free_port();
    let auth = with_auth.then(|| RegistryAuth {
        realm: format!("https://127.0.0.1:{port}/token"),
        username: "robot".into(),
        password: PASSWORD.into(),
        token: TOKEN.into(),
    });
    let service = serve_https_port(
        registry_app(REPO, registry_image(&image()), auth),
        &material,
        port,
    );
    (service, material, img)
}

fn host_config(ca: &std::path::Path, auth: Option<RegistryAuthSource>) -> RegistryHostConfig {
    RegistryHostConfig {
        insecure: false,
        ca_cert: Some(ca.to_path_buf()),
        auth,
    }
}

fn request(service: &Service, host: RegistryHostConfig) -> AcquireRequest {
    let registry = format!("127.0.0.1:{}", service.addr.port());
    AcquireRequest {
        tag: "awman-x-claude:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some("team/agent:1".into()),
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::from([(registry, host)]),
    }
}

fn env_auth() -> RegistryAuthSource {
    RegistryAuthSource::Env {
        username_var: "AWMAN_TEST_REG_USER".into(),
        password_var: "AWMAN_TEST_REG_PASS".into(),
    }
}

fn env(extra: &[(&str, &str)]) -> EnvSnapshot {
    let mut pairs = vec![
        ("AWMAN_TEST_REG_USER".to_string(), "robot".to_string()),
        ("AWMAN_TEST_REG_PASS".to_string(), PASSWORD.to_string()),
    ];
    pairs.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    EnvSnapshot::with_overrides(pairs)
}

#[test]
fn injected_environment_ignores_ambient_credentials_and_proxies() {
    const CHILD: &str = "AWMAN_TEST_SOURCE_ENV_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("config.json"), br#"{"auths":{}}"#).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "registry_store::injected_environment_ignores_ambient_credentials_and_proxies",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("DOCKER_CONFIG", temp.path())
            .env("HOME", temp.path());
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env(name, "http://127.0.0.1:9");
        }
        command.env("NO_PROXY", "").env("no_proxy", "");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let (service, material, _) = registry(false);
    let inherited_config =
        std::path::PathBuf::from(std::env::var_os("DOCKER_CONFIG").unwrap()).join("config.json");
    std::fs::write(
        &inherited_config,
        serde_json::to_vec(&serde_json::json!({"auths": {
            format!("127.0.0.1:{}", service.addr.port()): {"auth": "cm9ib3Q6cGFzcw=="}
        }}))
        .unwrap(),
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let acq = acquirer_with_env(temp.path(), EnvSnapshot::empty());
    acq.acquire(
        &request(&service, host_config(&material.ca_pem, None)),
        &mut |_| {},
    )
    .unwrap();
    let mut req = request(
        &service,
        host_config(&material.ca_pem, Some(RegistryAuthSource::DockerConfig)),
    );
    req.policy = AcquirePolicy::Refresh;
    let err = acq.acquire(&req, &mut |_| {}).unwrap_err();
    assert!(matches!(err, EngineError::Auth(_)), "{err}");
    assert!(err.to_string().contains("could not be read"), "{err}");
    // A verified hit needs neither the absent auth input nor the registry.
    drop(service);
    req.policy = AcquirePolicy::CachedOnly;
    acq.acquire(&req, &mut |_| {}).unwrap();
}

#[test]
fn retry_uses_credentials_captured_before_the_first_request() {
    use base64::Engine as _;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let temp = tempfile::tempdir().unwrap();
    let material = tls_material(&["localhost"], false);
    let port = free_port();
    let registry = format!("127.0.0.1:{port}");
    let config = temp.path().join("config.json");
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("robot:{PASSWORD}"));
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({"auths": {registry: {"auth": encoded}}})).unwrap(),
    )
    .unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let app = registry_app(
        REPO,
        registry_image(&image()),
        Some(RegistryAuth {
            realm: format!("https://127.0.0.1:{port}/token"),
            username: "robot".into(),
            password: PASSWORD.into(),
            token: TOKEN.into(),
        }),
    )
    .layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let config = config.clone();
            let seen = seen.clone();
            async move {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Future lookups fail. The retried acquisition must keep the
                    // credentials it prepared before making this first request.
                    std::fs::remove_file(config).unwrap();
                    return axum::response::IntoResponse::into_response(
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    );
                }
                next.run(request).await
            }
        },
    ));
    let service = serve_https_port(app, &material, port);
    let env = EnvSnapshot::with_overrides([("DOCKER_CONFIG", temp.path().display().to_string())]);
    let acq = acquirer_with_env(&temp.path().join("state"), env);
    acq.acquire(
        &request(
            &service,
            host_config(&material.ca_pem, Some(RegistryAuthSource::DockerConfig)),
        ),
        &mut |_| {},
    )
    .unwrap();
    assert!(requests.load(Ordering::SeqCst) > 2);
    assert!(!temp.path().join("config.json").exists());
}

#[test]
fn store_registry_ca_and_same_origin_token_realm_roundtrip() {
    let (service, material, img) = registry(true);
    let ca = material.ca_pem.clone();
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("s");
    let acq = acquirer_with_env(&state, env(&[]));
    let got = acq
        .acquire(
            &request(&service, host_config(&ca, Some(env_auth()))),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(got.identity.source, ImageSourceKind::Registry);
    assert_eq!(got.archive_format, ArchiveFormat::OciLayout);
    assert_eq!(
        got.identity.manifest_digest.as_str(),
        format!("sha256:{}", img.manifest_hex)
    );
    let seen = service.requests();
    let token = seen
        .iter()
        .find(|(p, _)| p.starts_with("/token"))
        .expect("the same-origin realm was asked for a token");
    assert!(
        token
            .1
            .get("authorization")
            .is_some_and(|a| a.starts_with("Basic ")),
        "credentials travel to the registry's own realm"
    );
    assert!(seen.iter().any(|(p, h)| p.contains("/blobs/")
        && h.get("authorization") == Some(&format!("Bearer {TOKEN}"))));

    // Offline reuse after the registry is gone.
    service.stop();
    let mut cached = request(&service, host_config(&ca, Some(env_auth())));
    cached.policy = AcquirePolicy::CachedOnly;
    assert_eq!(acq.acquire(&cached, &mut |_| {}).unwrap(), got);
}

#[test]
fn store_registry_wrong_ca_is_final_and_leaks_nothing() {
    let (service, _material, _) = registry(true);
    let material = tls_material(&["localhost"], false);
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("s");
    let err = acquirer_with_env(&state, env(&[]))
        .acquire(
            &request(
                &service,
                host_config(&material.other_ca_pem, Some(env_auth())),
            ),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Network(_)), "{err:?}");
    let text = err.to_string();
    assert!(!text.contains(PASSWORD) && !text.contains(TOKEN), "{text}");
    assert!(
        !text.contains("attempts against the same source"),
        "certificate failures are not retried: {text}"
    );
    assert!(
        service.requests().is_empty(),
        "the handshake never completed"
    );
    assert_eq!(cached_archives(&state), 0);
}

#[test]
fn registry_untrusted_realm_and_redirect_redact_sentinels() {
    // A registry whose challenge points at a foreign realm: the token is
    // requested there anonymously (never with the configured secret) and,
    // when that realm then refuses, the error carries no secret either.
    let material = tls_material(&["localhost"], false);
    let foreign = serve_https(
        axum::Router::new().route(
            "/token",
            axum::routing::get(|| async { (axum::http::StatusCode::UNAUTHORIZED, "no") }),
        ),
        &material,
    );
    let port = free_port();
    let auth = RegistryAuth {
        realm: format!("https://127.0.0.1:{}/token", foreign.addr.port()),
        username: "robot".into(),
        password: PASSWORD.into(),
        token: TOKEN.into(),
    };
    let service = serve_https_port(
        registry_app(REPO, registry_image(&image()), Some(auth)),
        &material,
        port,
    );
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("s");
    let err = acquirer_with_env(&state, env(&[]))
        .acquire(
            &request(&service, host_config(&material.ca_pem, Some(env_auth()))),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Auth(_)), "{err:?}");
    let text = err.to_string();
    assert!(!text.contains(PASSWORD) && !text.contains(TOKEN), "{text}");
    let foreign_seen = foreign.requests();
    assert!(!foreign_seen.is_empty(), "the foreign realm was contacted");
    for (_, headers) in &foreign_seen {
        assert!(
            !headers.contains_key("authorization"),
            "credentials never travel to a foreign realm"
        );
    }
    assert_eq!(cached_archives(&state), 0);
    // Auth failures are final: the registry saw exactly one challenge round.
    assert_eq!(
        service
            .requests()
            .iter()
            .filter(|(p, _)| p.contains("/manifests/"))
            .count(),
        1
    );
}

#[test]
fn store_registry_wrong_password_is_an_auth_error_without_the_secret() {
    let (service, material, _) = registry(true);
    let ca = material.ca_pem.clone();
    let temp = tempfile::tempdir().unwrap();
    let env = env(&[("AWMAN_TEST_REG_PASS", "SENTINEL-wrong-password-0000")]);
    let err = acquirer_with_env(&temp.path().join("s"), env)
        .acquire(
            &request(&service, host_config(&ca, Some(env_auth()))),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Auth(_)), "{err:?}");
    let text = err.to_string();
    assert!(!text.contains("SENTINEL-wrong-password"), "{text}");
    assert!(text.contains("environment variables"), "{text}");
    assert_eq!(
        service
            .requests()
            .iter()
            .filter(|(p, _)| p.starts_with("/token"))
            .count(),
        1,
        "a rejected credential is not retried"
    );
}

#[test]
fn store_registry_anonymous_access_needs_no_credentials_and_helpers_never_run() {
    let (service, material, img) = registry(false);
    let ca = material.ca_pem.clone();
    let temp = tempfile::tempdir().unwrap();
    let got = acquirer_with_env(&temp.path().join("s"), EnvSnapshot::empty())
        .acquire(&request(&service, host_config(&ca, None)), &mut |_| {})
        .unwrap();
    assert_eq!(
        got.identity.config_digest.as_str(),
        format!("sha256:{}", img.config_hex)
    );
    for (_, headers) in service.requests() {
        assert!(!headers.contains_key("authorization"));
    }

    // A Docker config that only names a credential helper is refused
    // before any request: awman never executes `docker-credential-*`.
    let config_dir = temp.path().join("docker-config");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.json"),
        r#"{"auths":{},"credsStore":"sentinel-helper"}"#,
    )
    .unwrap();
    let before = service.requests().len();
    let err = acquirer_with_env(
        &temp.path().join("t"),
        EnvSnapshot::with_overrides([("DOCKER_CONFIG", config_dir.display().to_string())]),
    )
    .acquire(
        &request(
            &service,
            host_config(&ca, Some(RegistryAuthSource::DockerConfig)),
        ),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(err, EngineError::Auth(ref m) if m.contains("docker-credential-sentinel-helper") && m.contains("never executes")),
        "{err:?}"
    );
    assert_eq!(
        service.requests().len(),
        before,
        "refused before contacting the registry"
    );
}

#[test]
fn store_registry_proxy_is_used_and_no_proxy_bypasses_it() {
    let (service, material, _) = registry(true);
    let ca = material.ca_pem.clone();
    let temp = tempfile::tempdir().unwrap();

    let proxy = connect_proxy(false);
    let got = acquirer_with_env(
        &temp.path().join("via-proxy"),
        env(&[("HTTPS_PROXY", &proxy.url())]),
    )
    .acquire(
        &request(&service, host_config(&ca, Some(env_auth()))),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(got.identity.reference, "team/agent:1");
    let tunnels = proxy.connects();
    assert!(
        tunnels
            .iter()
            .all(|(t, _)| t == &format!("127.0.0.1:{}", service.addr.port())),
        "{tunnels:?}"
    );
    assert!(
        !tunnels.is_empty(),
        "the registry was reached through the proxy"
    );

    let bypass = connect_proxy(false);
    acquirer_with_env(
        &temp.path().join("bypass"),
        env(&[
            ("HTTPS_PROXY", &bypass.url()),
            ("NO_PROXY", "127.0.0.1,localhost"),
        ]),
    )
    .acquire(
        &request(&service, host_config(&ca, Some(env_auth()))),
        &mut |_| {},
    )
    .unwrap();
    assert!(
        bypass.connects().is_empty(),
        "NO_PROXY hosts never touch the proxy"
    );

    // A proxy that refuses the tunnel is a network failure whose text does
    // not contain the proxy credential.
    let strict = connect_proxy(true);
    let err = acquirer_with_env(
        &temp.path().join("refused"),
        env(&[("HTTPS_PROXY", &strict.url())]),
    )
    .acquire(
        &request(&service, host_config(&ca, Some(env_auth()))),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(err, EngineError::Network(_)), "{err:?}");
    assert_eq!(cached_archives(&temp.path().join("refused")), 0);

    let with_creds = connect_proxy(true);
    let got = acquirer_with_env(
        &temp.path().join("creds"),
        env(&[(
            "HTTPS_PROXY",
            &with_creds.url_with_credentials("proxyuser", PROXY_PASSWORD),
        )]),
    )
    .acquire(
        &request(&service, host_config(&ca, Some(env_auth()))),
        &mut |_| {},
    );
    let tunnels = with_creds.connects();
    assert!(
        tunnels.iter().any(|(_, auth)| auth.is_some()),
        "proxy credentials were presented: {tunnels:?}"
    );
    match got {
        Ok(image) => assert_eq!(image.identity.source, ImageSourceKind::Registry),
        Err(err) => assert!(!err.to_string().contains(PROXY_PASSWORD), "{err}"),
    }
}

#[test]
fn a_proxy_that_is_unreachable_never_exposes_its_credentials() {
    let (service, material, _) = registry(true);
    let ca = material.ca_pem.clone();
    let temp = tempfile::tempdir().unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let err = acquirer_with_env(
        &temp.path().join("s"),
        env(&[(
            "HTTPS_PROXY",
            &format!("http://proxyuser:{PROXY_PASSWORD}@127.0.0.1:{port}"),
        )]),
    )
    .acquire(
        &request(&service, host_config(&ca, Some(env_auth()))),
        &mut |_| {},
    )
    .unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, EngineError::Network(_)), "{err:?}");
    assert!(!text.contains(PROXY_PASSWORD), "{text}");
    assert!(!text.contains(PASSWORD), "{text}");
    assert!(service.requests().is_empty());
}

#[test]
fn store_registry_local_insecure_registry_with_basic_auth() {
    // A plain-HTTP local registry marked `insecure`, answering with a Basic
    // challenge instead of Bearer.
    use axum::{extract::Request, http::StatusCode, response::IntoResponse};
    let img = registry_image(&image());
    let expected = format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("robot:{PASSWORD}")
        )
    );
    let img = std::sync::Arc::new(img);
    let handler = move |req: Request| {
        let img = img.clone();
        let expected = expected.clone();
        async move {
            let got = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if got != expected {
                return (
                    StatusCode::UNAUTHORIZED,
                    [("www-authenticate", r#"Basic realm="local""#)],
                    "unauthorized",
                )
                    .into_response();
            }
            let path = req.uri().path().to_string();
            if path.ends_with("/manifests/1") {
                return (
                    StatusCode::OK,
                    [("content-type", "application/vnd.oci.image.index.v1+json")],
                    img.index.clone(),
                )
                    .into_response();
            }
            if path.ends_with(&format!("/manifests/sha256:{}", img.manifest_hex)) {
                return (
                    StatusCode::OK,
                    [("content-type", "application/vnd.oci.image.manifest.v1+json")],
                    img.manifest.clone(),
                )
                    .into_response();
            }
            if path.ends_with(&format!("/blobs/sha256:{}", img.config_hex)) {
                return (StatusCode::OK, img.config.clone()).into_response();
            }
            for (hex, bytes) in &img.layers {
                if path.ends_with(&format!("/blobs/sha256:{hex}")) {
                    return (StatusCode::OK, bytes.clone()).into_response();
                }
            }
            (StatusCode::NOT_FOUND, "nope").into_response()
        }
    };
    let app = axum::Router::new().fallback(handler);
    let service = serve_http(app);
    let temp = tempfile::tempdir().unwrap();
    let registry = format!("127.0.0.1:{}", service.addr.port());
    let req = AcquireRequest {
        tag: "awman-x-claude:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some("team/agent:1".into()),
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::from([(
            registry,
            RegistryHostConfig {
                insecure: true,
                ca_cert: None,
                auth: Some(env_auth()),
            },
        )]),
    };
    let got = acquirer_with_env(&temp.path().join("s"), env(&[]))
        .acquire(&req, &mut |_| {})
        .unwrap();
    assert_eq!(got.identity.source, ImageSourceKind::Registry);
    assert!(service.requests().iter().any(|(_, h)| h
        .get("authorization")
        .is_some_and(|a| a.starts_with("Basic "))));
}
