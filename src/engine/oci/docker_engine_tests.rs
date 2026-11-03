//! Endpoint-resolution and reference-validation edge cases for the Docker
//! Engine source. The basic cases live in `engine::oci::tests::engine_store`;
//! these cover IPv6, ports, precedence, the TLS environment fallbacks and the
//! reference length and character boundaries. No daemon is contacted.

use super::*;

fn env(pairs: &[(&str, &str)]) -> EnvSnapshot {
    EnvSnapshot::with_overrides(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

fn tls() -> DockerTlsConfig {
    DockerTlsConfig {
        ca: "/ca.pem".into(),
        cert: None,
        key: None,
        verify: true,
    }
}

fn tcp(host: &str, port: u16, tls: Option<DockerTlsConfig>) -> Endpoint {
    Endpoint::Tcp {
        host: host.into(),
        port,
        tls,
    }
}

fn explicit(host: &str) -> Result<Endpoint, EngineError> {
    resolve_endpoint(Some(host), None, &EnvSnapshot::empty())
}

#[test]
fn loopback_forms_are_accepted_over_plain_tcp() {
    assert_eq!(
        explicit("tcp://localhost").unwrap(),
        tcp("localhost", 2375, None)
    );
    assert_eq!(
        explicit("tcp://127.3.4.5:1").unwrap(),
        tcp("127.3.4.5", 1, None)
    );
    assert_eq!(
        explicit("tcp://[::1]:2400").unwrap(),
        tcp("::1", 2400, None)
    );
    assert_eq!(explicit("tcp://[::1]").unwrap(), tcp("::1", 2375, None));
    // Surrounding whitespace and a trailing slash are tolerated.
    assert_eq!(
        explicit("  tcp://127.0.0.1:2375/  ").unwrap(),
        tcp("127.0.0.1", 2375, None)
    );
}

#[test]
fn non_loopback_lookalikes_are_refused_over_plain_tcp() {
    for host in [
        "tcp://0.0.0.0:2375",
        "tcp://localhost.example.com",
        "tcp://LOCALHOST",
        "tcp://[::ffff:127.0.0.1]",
        "tcp://[fe80::1]:2375",
        "tcp://192.168.1.10",
    ] {
        assert!(
            matches!(
                explicit(host),
                Err(EngineError::UnsupportedImageSource { .. })
            ),
            "{host}"
        );
    }
}

#[test]
fn remote_hosts_with_tls_default_to_2376_and_keep_ipv6_brackets_out() {
    let t = tls();
    let e = EnvSnapshot::empty();
    assert_eq!(
        resolve_endpoint(Some("tcp://[2001:db8::1]"), Some(&t), &e).unwrap(),
        tcp("2001:db8::1", 2376, Some(t.clone()))
    );
    assert_eq!(
        resolve_endpoint(Some("tcp://10.0.0.5:9999"), Some(&t), &e).unwrap(),
        tcp("10.0.0.5", 9999, Some(t.clone()))
    );
}

#[test]
fn malformed_tcp_hosts_are_config_errors() {
    for host in [
        "tcp://",
        "tcp://127.0.0.1:",
        "tcp://127.0.0.1:99999",
        "tcp://127.0.0.1:port",
        "tcp://127.0.0.1/path",
        "tcp://[::1",
        "tcp://[::1]x",
        "tcp://[::1]:x",
        "tcp://user@127.0.0.1",
    ] {
        assert!(
            matches!(explicit(host), Err(EngineError::Config(_))),
            "{host}"
        );
    }
}

#[test]
fn unix_hosts_must_be_absolute_and_never_take_tls() {
    assert!(matches!(
        explicit("unix://run/docker.sock"),
        Err(EngineError::Config(_))
    ));
    assert!(matches!(explicit("unix://"), Err(EngineError::Config(_))));
    assert!(matches!(
        resolve_endpoint(Some("unix:///d.sock"), Some(&tls()), &EnvSnapshot::empty()),
        Err(EngineError::Config(_))
    ));
}

#[test]
fn unknown_schemes_are_config_errors() {
    for host in [
        "http://127.0.0.1:2375",
        "127.0.0.1:2375",
        "/var/run/docker.sock",
        "fd://",
    ] {
        assert!(
            matches!(explicit(host), Err(EngineError::Config(_))),
            "{host}"
        );
    }
}

#[test]
fn an_explicit_host_beats_docker_host_and_ignores_its_tls_env() {
    let e = env(&[
        ("DOCKER_HOST", "tcp://docker.example:2376"),
        ("DOCKER_TLS_VERIFY", "1"),
        ("DOCKER_CERT_PATH", "/certs"),
    ]);
    assert_eq!(
        resolve_endpoint(Some("unix:///explicit.sock"), None, &e).unwrap(),
        Endpoint::Unix("/explicit.sock".into())
    );
    // The DOCKER_TLS_* variables belong to DOCKER_HOST only, so an explicit
    // remote host without its own `tls` stays refused.
    assert!(matches!(
        resolve_endpoint(Some("tcp://docker.example:2376"), None, &e),
        Err(EngineError::UnsupportedImageSource { .. })
    ));
}

#[test]
fn source_tls_beats_the_tls_environment() {
    let e = env(&[
        ("DOCKER_HOST", "tcp://docker.example"),
        ("DOCKER_TLS_VERIFY", "1"),
        ("DOCKER_CERT_PATH", "/certs"),
    ]);
    let t = tls();
    assert_eq!(
        resolve_endpoint(None, Some(&t), &e).unwrap(),
        tcp("docker.example", 2376, Some(t))
    );
}

#[test]
fn tls_verify_without_cert_path_uses_home_docker_or_fails_without_home() {
    let e = env(&[
        ("DOCKER_HOST", "tcp://docker.example"),
        ("DOCKER_TLS_VERIFY", "yes"),
        ("HOME", "/home/me"),
    ]);
    let Endpoint::Tcp {
        port, tls: Some(t), ..
    } = resolve_endpoint(None, None, &e).unwrap()
    else {
        panic!("expected a TLS endpoint");
    };
    assert_eq!(port, 2376);
    assert_eq!(t.ca, PathBuf::from("/home/me/.docker/ca.pem"));
    assert_eq!(t.cert, Some(PathBuf::from("/home/me/.docker/cert.pem")));
    assert!(t.verify);

    let e = env(&[
        ("DOCKER_HOST", "tcp://docker.example"),
        ("DOCKER_TLS_VERIFY", "1"),
    ]);
    assert!(matches!(
        resolve_endpoint(None, None, &e),
        Err(EngineError::Config(_))
    ));
}

#[test]
fn blank_tls_verify_and_blank_docker_host_are_unset() {
    // A whitespace DOCKER_TLS_VERIFY does not enable TLS: remote plain tcp
    // stays refused rather than silently gaining a TLS config.
    let e = env(&[
        ("DOCKER_HOST", "tcp://docker.example"),
        ("DOCKER_TLS_VERIFY", "  "),
        ("DOCKER_CERT_PATH", "/certs"),
    ]);
    assert!(matches!(
        resolve_endpoint(None, None, &e),
        Err(EngineError::UnsupportedImageSource { .. })
    ));
    // A blank DOCKER_HOST falls through to the default socket, which test
    // isolation keeps off-limits.
    assert!(matches!(
        resolve_endpoint(None, None, &env(&[("DOCKER_HOST", "   ")])),
        Err(EngineError::UnsupportedImageSource { .. })
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn default_socket_is_the_system_socket_when_docker_tests_opt_in() {
    // Resolution only names the path; nothing connects to it.
    assert_eq!(
        resolve_endpoint(None, None, &env(&[("AWMAN_TEST_DOCKER", "1")])).unwrap(),
        Endpoint::Unix("/var/run/docker.sock".into())
    );
}

#[test]
fn describe_never_includes_tls_material() {
    assert_eq!(
        Endpoint::Unix("/d.sock".into()).describe(),
        "unix:///d.sock"
    );
    assert_eq!(tcp("::1", 2375, None).describe(), "tcp://::1:2375");
    let t = DockerTlsConfig {
        ca: "/secret/ca.pem".into(),
        cert: Some("/secret/cert.pem".into()),
        key: Some("/secret/key.pem".into()),
        verify: false,
    };
    let d = tcp("docker.example", 2376, Some(t)).describe();
    assert_eq!(d, "tcp+tls://docker.example:2376");
}

#[test]
fn reference_length_and_leading_character_boundaries() {
    let max = "a".repeat(512);
    assert!(validate_reference(&max).is_ok());
    assert!(validate_reference(&format!("{max}a")).is_err());
    for bad in [
        "-x", ".x", "x/..", "a..b", "x%2e%2e", "x\n", "x\\y", "ïmage", "x;y", "x&y",
    ] {
        assert!(validate_reference(bad).is_err(), "{bad:?}");
    }
    for good in [
        "x",
        "ghcr.io/org/img:v1.2.3",
        "img@sha256:0123456789abcdef",
        "a_b-c.d/e",
    ] {
        assert!(validate_reference(good).is_ok(), "{good:?}");
    }
}

#[test]
fn reference_error_is_a_config_error_naming_the_reference() {
    let err = validate_reference("../x").unwrap_err();
    assert!(
        matches!(err, EngineError::Config(ref m) if m.contains("`../x`")),
        "{err:?}"
    );
}

#[test]
fn api_versions_parse_strictly() {
    assert_eq!(parse_api("1.41"), Some((1, 41)));
    assert_eq!(parse_api(" 1.48 "), Some((1, 48)));
    assert_eq!(parse_api("1"), None);
    assert_eq!(parse_api("1.x"), None);
    assert_eq!(parse_api("1.41.2"), None);
    assert!(parse_api("1.40").unwrap() < MIN_API);
    assert!(parse_api("1.100").unwrap() > PLATFORM_API);
}
