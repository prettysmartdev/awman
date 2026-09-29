use strict; use warnings;
my $path = 'tests/oci_import/real_stores.rs';
open my $in, '<', $path or die $!; local $/; my $s = <$in>; close $in;
my $start = index($s, '#[test]\nfn store_docker_real_engine_unix_or_tls_roundtrip');
$start = index($s, "#[test]\nfn store_docker_real_engine_unix_or_tls_roundtrip");
my $end = index($s, "#[test]\nfn store_registry_real_ca_proxy_auth_roundtrip", $start);
die 'range' if $start < 0 || $end < 0;
my $replacement = <<'RUST';
#[test]
fn store_docker_real_engine_unix_roundtrip() {
    docker_roundtrip("store_docker_real_engine_unix_roundtrip", "AWMAN_TEST_DOCKER_STORE_HOST", false, false);
}

#[test]
fn store_docker_real_engine_tls_roundtrip() {
    docker_roundtrip("store_docker_real_engine_tls_roundtrip", "AWMAN_TEST_DOCKER_STORE_TLS_HOST", true, false);
}

#[test]
fn store_docker_real_engine_required_mtls_roundtrip() {
    docker_roundtrip("store_docker_real_engine_required_mtls_roundtrip", "AWMAN_TEST_DOCKER_STORE_MTLS_HOST", true, true);
}

fn docker_roundtrip(test: &'static str, endpoint_key: &str, use_tls: bool, mutual: bool) {
    let mut required = vec![endpoint_key, "AWMAN_TEST_DOCKER_STORE_IMAGE"];
    if use_tls { required.push("AWMAN_TEST_DOCKER_STORE_CA"); }
    if mutual { required.extend(["AWMAN_TEST_DOCKER_STORE_CERT", "AWMAN_TEST_DOCKER_STORE_KEY"]); }
    let Some(inputs) = gated(test, "AWMAN_TEST_DOCKER", &required) else { return };
    let report = ScenarioReport::start(test);
    let host = inputs[endpoint_key].clone();
    assert!(if use_tls { host.starts_with("tcp://") || host.starts_with("https://") } else { host.starts_with("unix://") }, "endpoint must match the named transport scenario");
    let reference = inputs["AWMAN_TEST_DOCKER_STORE_IMAGE"].clone();
    let tls = use_tls.then(|| DockerTlsConfig {
        ca: PathBuf::from(&inputs["AWMAN_TEST_DOCKER_STORE_CA"]),
        cert: mutual.then(|| PathBuf::from(&inputs["AWMAN_TEST_DOCKER_STORE_CERT"])),
        key: mutual.then(|| PathBuf::from(&inputs["AWMAN_TEST_DOCKER_STORE_KEY"])),
        verify: true,
    });
    let temp = tempfile::tempdir().unwrap();
    let request = |policy, reference: &str| AcquireRequest {
        tag: "awman-real-store:latest".into(),
        source: ImageSourceSpec::DockerStore {
            host: Some(host.clone()), tls: tls.clone(), reference: Some(reference.to_string()),
        },
        platform: OciPlatform::host_linux(), policy, registries: BTreeMap::new(),
    };
    let acq = acquirer(&temp.path().join("state"));
    let got = acq.acquire(&request(AcquirePolicy::IfMissing, &reference), &mut |_| {})
        .unwrap_or_else(|e| panic!("{test}: export failed: {e}"));
    assert_eq!(got.identity.source, ImageSourceKind::DockerStore);
    assert_eq!(got.identity.platform, OciPlatform::host_linux());
    let cached = acq.acquire(&request(AcquirePolicy::CachedOnly, &reference), &mut |_| {}).unwrap();
    assert_eq!(cached, got);
    if mutual {
        let mut without_identity = request(AcquirePolicy::Refresh, &reference);
        if let ImageSourceSpec::DockerStore { tls: Some(tls), .. } = &mut without_identity.source {
            tls.cert = None; tls.key = None;
        }
        let error = acquirer(&temp.path().join("no-client-identity"))
            .acquire(&without_identity, &mut |_| {}).unwrap_err();
        assert!(matches!(error, EngineError::Container(_)), "mTLS must be rejected by the real server, not by malformed input: {error}");
    }
    let err = acq.acquire(&request(AcquirePolicy::IfMissing, "awman-test/does-not-exist:none"), &mut |_| {}).unwrap_err();
    assert!(matches!(err, EngineError::Container(_)), "{err:?}");
    assert!(err.to_string().contains("does-not-exist"));
    report.pass(&format!("export, cached reuse, missing-image finality{}: {} ({} bytes)", if mutual { ", server rejects absent client identity" } else { "" }, got.identity.manifest_digest, got.bytes));
}

RUST
substr($s,$start,$end-$start)=$replacement;
open my $out, '>', $path or die $!; print $out $s;
