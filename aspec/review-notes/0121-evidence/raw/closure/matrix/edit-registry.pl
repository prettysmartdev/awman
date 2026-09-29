use strict; use warnings;
my $path='tests/oci_import/real_stores.rs'; open my $in,'<',$path or die $!; local $/; my $s=<$in>; close $in;
my $start=index($s,"#[test]\nfn store_registry_real_ca_proxy_auth_roundtrip"); my $end=index($s,'/// The host keychain',$start); die if $start<0||$end<0;
my $replacement=<<'RUST';
#[test]
fn store_registry_real_ca_proxy_auth_roundtrip() {
    use awman::data::config::env::EnvSnapshot;
    use std::io::{Read as _, Seek as _};
    const TEST: &str = "store_registry_real_ca_proxy_auth_roundtrip";
    let Some(inputs) = gated(TEST, "AWMAN_TEST_REGISTRY", &[
        "AWMAN_TEST_REGISTRY_HOST", "AWMAN_TEST_REGISTRY_REFERENCE", "AWMAN_TEST_REGISTRY_CA",
        "AWMAN_TEST_REGISTRY_USERNAME_VAR", "AWMAN_TEST_REGISTRY_PASSWORD_VAR",
        "AWMAN_TEST_REGISTRY_PROXY_URL", "AWMAN_TEST_REGISTRY_PROXY_LOG",
    ]) else { return };
    let report = ScenarioReport::start(TEST);
    assert!(!truthy("AWMAN_TEST_REGISTRY_INSECURE"), "private-CA scenario requires verified HTTPS");
    let registry = inputs["AWMAN_TEST_REGISTRY_HOST"].clone();
    let reference = inputs["AWMAN_TEST_REGISTRY_REFERENCE"].clone();
    let username_var = inputs["AWMAN_TEST_REGISTRY_USERNAME_VAR"].clone();
    let password_var = inputs["AWMAN_TEST_REGISTRY_PASSWORD_VAR"].clone();
    let username = optional(&username_var).expect("configured registry username variable is absent");
    let password = optional(&password_var).expect("configured registry password variable is absent");
    let host = RegistryHostConfig {
        insecure: false,
        ca_cert: Some(PathBuf::from(&inputs["AWMAN_TEST_REGISTRY_CA"])),
        auth: Some(RegistryAuthSource::Env { username_var: username_var.clone(), password_var: password_var.clone() }),
    };
    let env = EnvSnapshot::from_pairs([
        (username_var, username), (password_var, password),
        ("HTTPS_PROXY".into(), inputs["AWMAN_TEST_REGISTRY_PROXY_URL"].clone()),
        ("NO_PROXY".into(), String::new()),
    ]);
    // The disposable real proxy must log CONNECT authorities. Snapshot its
    // end before the pull: an old run's log is not evidence for this request.
    let mut proxy_log = std::fs::File::open(&inputs["AWMAN_TEST_REGISTRY_PROXY_LOG"])
        .expect("open the disposable proxy's access log");
    proxy_log.seek(std::io::SeekFrom::End(0)).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let request = |policy| AcquireRequest {
        tag: "awman-real-registry:latest".into(),
        source: ImageSourceSpec::Registry { registry: Some(registry.clone()), reference: Some(reference.clone()) },
        platform: OciPlatform::host_linux(), policy,
        registries: BTreeMap::from([(registry.clone(), host.clone())]),
    };
    let acq = acquirer_with_env(&temp.path().join("state"), env.clone());
    let got = acq.acquire(&request(AcquirePolicy::IfMissing), &mut |_| {})
        .unwrap_or_else(|e| panic!("{TEST}: pull failed: {e}"));
    assert_eq!(got.identity.source, ImageSourceKind::Registry);
    // Wait a bounded interval for a line-buffered real proxy access log.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut observed = String::new();
    loop {
        proxy_log.read_to_string(&mut observed).unwrap();
        if observed.lines().any(|line| line.contains("CONNECT") && line.contains(&registry)) { break; }
        assert!(std::time::Instant::now() < until, "no new CONNECT for the registry appeared in the real proxy log");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(acq.acquire(&request(AcquirePolicy::CachedOnly), &mut |_| {}).unwrap(), got);
    let mut anonymous = request(AcquirePolicy::Refresh);
    anonymous.registries.get_mut(&registry).unwrap().auth = None;
    let error = acquirer_with_env(&temp.path().join("anonymous"), env.clone())
        .acquire(&anonymous, &mut |_| {}).unwrap_err();
    let detail = error.to_string().to_ascii_lowercase();
    assert!(detail.contains("authentication") || detail.contains("unauthorized") || detail.contains("forbidden"), "anonymous pull must be rejected by the registry's auth contract: {error}");
    let mut untrusted = request(AcquirePolicy::Refresh);
    untrusted.registries.get_mut(&registry).unwrap().ca_cert = None;
    let error = acquirer_with_env(&temp.path().join("untrusted"), env)
        .acquire(&untrusted, &mut |_| {}).unwrap_err();
    assert!(matches!(error, EngineError::Container(_)), "private CA must be required by an actual TLS handshake: {error}");
    report.pass(&format!("authenticated private-CA pull through observed proxy CONNECT, cached reuse, anonymous and untrusted-CA refusals: {} ({} bytes)", got.identity.manifest_digest, got.bytes));
}

RUST
substr($s,$start,$end-$start)=$replacement;
# All keychain assertions including removal precede PASS.
my $old=<<'OLD';
    record(
        TEST,
        "PASS",
        &format!("pulled {reference} with credentials from keychain service {service}"),
    );
OLD
$s =~ s/\Q$old\E// or die 'keychain record';
$s =~ s/(    assert!\(entry.present\(\), "the disposable entry was stored"\);)/    let report = ScenarioReport::start(TEST);\n$1/ or die;
$s =~ s/(    assert!\(!still_there, "the disposable entry was removed"\);)/$1\n    report.pass(\&format!("pulled {reference} with disposable keychain credentials and verified removal"));/ or die;
my $record_start=index($s,'fn record('); my $record_end=index($s,'/// A durable whole-scenario', $record_start); substr($s,$record_start,$record_end-$record_start)='';
open my $out,'>',$path or die $!; print $out $s;
