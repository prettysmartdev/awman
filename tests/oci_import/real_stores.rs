//! Real disposable-service tier (opt-in). These tests contact services the
//! caller provisioned for this run — never a production daemon, registry or
//! the developer's credential store — and every input is explicit:
//!
//! | gate | inputs | what runs |
//! |---|---|---|
//! | `AWMAN_TEST_IMAGE_STORES=1` + `AWMAN_TEST_DOCKER=1` | `AWMAN_TEST_DOCKER_STORE_HOST` (Unix), `AWMAN_TEST_DOCKER_STORE_TLS_HOST`, `AWMAN_TEST_DOCKER_STORE_MTLS_HOST`, shared `AWMAN_TEST_DOCKER_STORE_IMAGE/CA/CERT/KEY` | Three required scenarios: Unix, TLS, and server-enforced mTLS; each verifies export, cached reuse and missing-image finality |
//! | `AWMAN_TEST_IMAGE_STORES=1` + `AWMAN_TEST_REGISTRY=1` | `AWMAN_TEST_REGISTRY_HOST/REFERENCE/CA`, `AWMAN_TEST_REGISTRY_USERNAME_VAR/PASSWORD_VAR` (names of variables holding credentials), `AWMAN_TEST_REGISTRY_PROXY_URL/PROXY_LOG` | Verified HTTPS private-CA/auth pull through a real proxy with newly observed CONNECT log records; anonymous and untrusted-CA refusal controls |
//! | `AWMAN_TEST_NATIVE_KEYCHAIN=1` (never under `AWMAN_TEST_ISOLATION`) | `AWMAN_TEST_REGISTRY_*` as above plus credentials in `AWMAN_TEST_KEYCHAIN_USERNAME`/`PASSWORD` | stores a disposable entry under a namespaced service in the host keychain, pulls with `{"type":"keychain"}`, removes the entry, verifies removal |
//!
//! Without a gate a test reports SKIP and passes nothing; with
//! `AWMAN_TEST_REQUIRE_EXTERNAL=1` a missing gate or input is a failure, so
//! a CI job that was meant to run these can never go green by skipping.

use std::collections::BTreeMap;
use std::path::PathBuf;

use awman::data::config::image_source::{
    DockerTlsConfig, ImageSourceKind, ImageSourceSpec, RegistryAuthSource, RegistryHostConfig,
};
use awman::data::oci_identity::OciPlatform;
use awman::engine::error::EngineError;
use awman::engine::oci::{AcquirePolicy, AcquireRequest, ImageAcquirer};

fn acquirer(state: &std::path::Path) -> awman::engine::oci::CachingAcquirer {
    acquirer_with_env(state, awman::data::config::env::EnvSnapshot::empty())
}

fn acquirer_with_env(
    state: &std::path::Path,
    env: awman::data::config::env::EnvSnapshot,
) -> awman::engine::oci::CachingAcquirer {
    // Real shipped images exceed the hermetic fixture's 64 MiB layer limit.
    // Exercise production capacity and free-space checks in this tier.
    awman::engine::oci::default_acquirer(state, awman::engine::oci::AcquireLimits::DEFAULT, &env)
}

fn truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Report SKIP (or fail under REQUIRE_EXTERNAL) and return `false`.
fn skip(test: &str, reason: &str) -> bool {
    let required = truthy("AWMAN_TEST_REQUIRE_EXTERNAL");
    let outcome = if required { "BLOCKED" } else { "SKIP" };
    eprintln!("{outcome}: {test}: {reason}");
    if required {
        panic!(
            "{test}: an external service is required (AWMAN_TEST_REQUIRE_EXTERNAL=1) but: {reason}"
        );
    }
    false
}

fn gated(test: &str, gate: &str, inputs: &[&str]) -> Option<BTreeMap<String, String>> {
    if !truthy("AWMAN_TEST_IMAGE_STORES") {
        skip(
            test,
            "AWMAN_TEST_IMAGE_STORES=1 is not set (real-store opt-in)",
        );
        return None;
    }
    if !truthy(gate) {
        skip(test, &format!("{gate}=1 is not set"));
        return None;
    }
    let mut out = BTreeMap::new();
    for name in inputs {
        match std::env::var(name) {
            Ok(v) if !v.trim().is_empty() => {
                out.insert(name.to_string(), v);
            }
            _ => {
                skip(test, &format!("{name} is not set"));
                return None;
            }
        }
    }
    Some(out)
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// A durable whole-scenario result is emitted only after the last assertion.
/// Unwinding (including a failed cache/cleanup assertion) records FAIL instead.
struct ScenarioReport {
    test: &'static str,
    path: Option<PathBuf>,
    complete: bool,
}

impl ScenarioReport {
    fn start(test: &'static str) -> Self {
        Self::at(
            test,
            std::env::var_os("AWMAN_TEST_STORES_REPORT").map(PathBuf::from),
        )
    }

    fn at(test: &'static str, path: Option<PathBuf>) -> Self {
        let report = Self {
            test,
            path,
            complete: false,
        };
        report.write("RUN", "scenario started");
        report
    }

    fn write(&self, outcome: &str, detail: &str) {
        use std::io::Write as _;
        eprintln!("{outcome}: {}: {detail}", self.test);
        if let Some(path) = &self.path {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open required store evidence report");
            writeln!(
                file,
                "{outcome}\t{}\t{}",
                self.test,
                detail.replace('\n', " ")
            )
            .expect("write required store evidence report");
        }
    }

    fn pass(mut self, detail: &str) {
        self.write("PASS", detail);
        self.complete = true;
    }
}

impl Drop for ScenarioReport {
    fn drop(&mut self) {
        if !self.complete {
            // Do not panic twice while reporting an assertion failure.
            let _ = std::panic::catch_unwind(|| self.write("FAIL", "scenario did not complete"));
        }
    }
}

#[test]
fn late_stage_failure_never_writes_whole_scenario_pass() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("report.tsv");
    let failed = std::panic::catch_unwind(|| {
        let report = ScenarioReport::at("late-stage", Some(path.clone()));
        let exported = temp.path().join("exported-archive");
        std::fs::write(&exported, "initial stage artifact").unwrap();
        assert!(exported.is_file());
        let cached = std::fs::read_to_string(exported).unwrap();
        assert_eq!(cached, "verified-cache-hit");
        report.pass("all assertions completed");
    });
    assert!(failed.is_err());
    let contents = std::fs::read_to_string(path).unwrap();
    assert!(contents.contains("RUN\tlate-stage\t"));
    assert!(contents.contains("FAIL\tlate-stage\t"));
    assert!(!contents.contains("PASS\t"), "{contents}");
}

#[test]
fn store_docker_real_engine_unix_roundtrip() {
    docker_roundtrip(
        "store_docker_real_engine_unix_roundtrip",
        "AWMAN_TEST_DOCKER_STORE_HOST",
        false,
        false,
    );
}

#[test]
fn store_docker_real_engine_tls_roundtrip() {
    docker_roundtrip(
        "store_docker_real_engine_tls_roundtrip",
        "AWMAN_TEST_DOCKER_STORE_TLS_HOST",
        true,
        false,
    );
}

#[test]
fn store_docker_real_engine_required_mtls_roundtrip() {
    docker_roundtrip(
        "store_docker_real_engine_required_mtls_roundtrip",
        "AWMAN_TEST_DOCKER_STORE_MTLS_HOST",
        true,
        true,
    );
}

fn docker_roundtrip(test: &'static str, endpoint_key: &str, use_tls: bool, mutual: bool) {
    let mut required = vec![endpoint_key, "AWMAN_TEST_DOCKER_STORE_IMAGE"];
    if use_tls {
        required.push("AWMAN_TEST_DOCKER_STORE_CA");
    }
    if mutual {
        required.extend([
            "AWMAN_TEST_DOCKER_STORE_CERT",
            "AWMAN_TEST_DOCKER_STORE_KEY",
        ]);
    }
    let Some(inputs) = gated(test, "AWMAN_TEST_DOCKER", &required) else {
        return;
    };
    let report = ScenarioReport::start(test);
    let host = inputs[endpoint_key].clone();
    assert!(
        if use_tls {
            host.starts_with("tcp://") || host.starts_with("https://")
        } else {
            host.starts_with("unix://")
        },
        "endpoint must match the named transport scenario"
    );
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
            host: Some(host.clone()),
            tls: tls.clone(),
            reference: Some(reference.to_string()),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: BTreeMap::new(),
    };
    let acq = acquirer(&temp.path().join("state"));
    let got = acq
        .acquire(&request(AcquirePolicy::IfMissing, &reference), &mut |_| {})
        .unwrap_or_else(|e| panic!("{test}: export failed: {e}"));
    assert_eq!(got.identity.source, ImageSourceKind::DockerStore);
    assert_eq!(got.identity.platform, OciPlatform::host_linux());
    let cached = acq
        .acquire(&request(AcquirePolicy::CachedOnly, &reference), &mut |_| {})
        .unwrap();
    assert_eq!(cached, got);
    if mutual {
        let mut without_identity = request(AcquirePolicy::Refresh, &reference);
        if let ImageSourceSpec::DockerStore { tls: Some(tls), .. } = &mut without_identity.source {
            tls.cert = None;
            tls.key = None;
        }
        let error = acquirer(&temp.path().join("no-client-identity"))
            .acquire(&without_identity, &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(error, EngineError::Container(_)),
            "mTLS must be rejected by the real server, not by malformed input: {error}"
        );
    }
    let err = acq
        .acquire(
            &request(AcquirePolicy::IfMissing, "awman-test/does-not-exist:none"),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Container(_)), "{err:?}");
    assert!(err.to_string().contains("does-not-exist"));
    report.pass(&format!(
        "export, cached reuse, missing-image finality{}: {} ({} bytes)",
        if mutual {
            ", server rejects absent client identity"
        } else {
            ""
        },
        got.identity.manifest_digest,
        got.bytes
    ));
}

#[test]
fn store_registry_real_ca_proxy_auth_roundtrip() {
    use awman::data::config::env::EnvSnapshot;
    use std::io::{Read as _, Seek as _};
    const TEST: &str = "store_registry_real_ca_proxy_auth_roundtrip";
    let Some(inputs) = gated(
        TEST,
        "AWMAN_TEST_REGISTRY",
        &[
            "AWMAN_TEST_REGISTRY_HOST",
            "AWMAN_TEST_REGISTRY_REFERENCE",
            "AWMAN_TEST_REGISTRY_CA",
            "AWMAN_TEST_REGISTRY_USERNAME_VAR",
            "AWMAN_TEST_REGISTRY_PASSWORD_VAR",
            "AWMAN_TEST_REGISTRY_PROXY_URL",
            "AWMAN_TEST_REGISTRY_PROXY_LOG",
        ],
    ) else {
        return;
    };
    let report = ScenarioReport::start(TEST);
    assert!(
        !truthy("AWMAN_TEST_REGISTRY_INSECURE"),
        "private-CA scenario requires verified HTTPS"
    );
    let registry = inputs["AWMAN_TEST_REGISTRY_HOST"].clone();
    let reference = inputs["AWMAN_TEST_REGISTRY_REFERENCE"].clone();
    let username_var = inputs["AWMAN_TEST_REGISTRY_USERNAME_VAR"].clone();
    let password_var = inputs["AWMAN_TEST_REGISTRY_PASSWORD_VAR"].clone();
    let username =
        optional(&username_var).expect("configured registry username variable is absent");
    let password =
        optional(&password_var).expect("configured registry password variable is absent");
    let host = RegistryHostConfig {
        insecure: false,
        ca_cert: Some(PathBuf::from(&inputs["AWMAN_TEST_REGISTRY_CA"])),
        auth: Some(RegistryAuthSource::Env {
            username_var: username_var.clone(),
            password_var: password_var.clone(),
        }),
    };
    let env = EnvSnapshot::with_overrides([
        (username_var, username),
        (password_var, password),
        (
            "HTTPS_PROXY".into(),
            inputs["AWMAN_TEST_REGISTRY_PROXY_URL"].clone(),
        ),
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
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some(reference.clone()),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: BTreeMap::from([(registry.clone(), host.clone())]),
    };
    let acq = acquirer_with_env(&temp.path().join("state"), env.clone());
    let got = acq
        .acquire(&request(AcquirePolicy::IfMissing), &mut |_| {})
        .unwrap_or_else(|e| panic!("{TEST}: pull failed: {e}"));
    assert_eq!(got.identity.source, ImageSourceKind::Registry);
    // Wait a bounded interval for a line-buffered real proxy access log.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut observed = String::new();
    loop {
        proxy_log.read_to_string(&mut observed).unwrap();
        if observed
            .lines()
            .any(|line| line.contains("CONNECT") && line.contains(&registry))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "no new CONNECT for the registry appeared in the real proxy log"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        acq.acquire(&request(AcquirePolicy::CachedOnly), &mut |_| {})
            .unwrap(),
        got
    );
    let mut anonymous = request(AcquirePolicy::Refresh);
    anonymous.registries.get_mut(&registry).unwrap().auth = None;
    let error = acquirer_with_env(&temp.path().join("anonymous"), env.clone())
        .acquire(&anonymous, &mut |_| {})
        .unwrap_err();
    let detail = error.to_string().to_ascii_lowercase();
    assert!(
        detail.contains("authentication")
            || detail.contains("unauthorized")
            || detail.contains("forbidden"),
        "anonymous pull must be rejected by the registry's auth contract: {error}"
    );
    let mut untrusted = request(AcquirePolicy::Refresh);
    untrusted.registries.get_mut(&registry).unwrap().ca_cert = None;
    let error = acquirer_with_env(&temp.path().join("untrusted"), env)
        .acquire(&untrusted, &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(error, EngineError::Container(_)),
        "private CA must be required by an actual TLS handshake: {error}"
    );
    report.pass(&format!("authenticated private-CA pull through observed proxy CONNECT, cached reuse, anonymous and untrusted-CA refusals: {} ({} bytes)", got.identity.manifest_digest, got.bytes));
}

/// The host keychain, driven the way awman drives it: `security` on macOS
/// and `secret-tool` on Linux. Entries are namespaced per run and removed
/// afterwards; nothing else in the store is touched.
struct DisposableKeychainEntry {
    service: String,
    account: String,
}

impl DisposableKeychainEntry {
    fn store(service: &str, account: &str, value: &str) -> Result<Self, String> {
        let status = if cfg!(target_os = "macos") {
            std::process::Command::new("security")
                .args([
                    "add-generic-password",
                    "-U",
                    "-s",
                    service,
                    "-a",
                    account,
                    "-w",
                    value,
                ])
                .status()
        } else {
            use std::io::Write as _;
            let mut child = std::process::Command::new("secret-tool")
                .args([
                    "store", "--label", service, "service", service, "account", account,
                ])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(value.as_bytes())
                .map_err(|e| e.to_string())?;
            child.wait()
        }
        .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("storing the disposable entry failed: {status}"));
        }
        Ok(Self {
            service: service.into(),
            account: account.into(),
        })
    }

    fn present(&self) -> bool {
        let out = if cfg!(target_os = "macos") {
            std::process::Command::new("security")
                .args([
                    "find-generic-password",
                    "-s",
                    &self.service,
                    "-a",
                    &self.account,
                ])
                .output()
        } else {
            std::process::Command::new("secret-tool")
                .args(["lookup", "service", &self.service, "account", &self.account])
                .output()
        };
        out.map(|o| o.status.success() && (cfg!(target_os = "macos") || !o.stdout.is_empty()))
            .unwrap_or(false)
    }
}

impl Drop for DisposableKeychainEntry {
    fn drop(&mut self) {
        let _ = if cfg!(target_os = "macos") {
            std::process::Command::new("security")
                .args([
                    "delete-generic-password",
                    "-s",
                    &self.service,
                    "-a",
                    &self.account,
                ])
                .output()
        } else {
            std::process::Command::new("secret-tool")
                .args(["clear", "service", &self.service, "account", &self.account])
                .output()
        };
    }
}

#[test]
fn native_keychain_disposable_registry_entry() {
    const TEST: &str = "native_keychain_disposable_registry_entry";
    if !truthy("AWMAN_TEST_NATIVE_KEYCHAIN") {
        skip(
            TEST,
            "AWMAN_TEST_NATIVE_KEYCHAIN=1 is not set (native credential opt-in)",
        );
        return;
    }
    if std::env::var_os("AWMAN_TEST_ISOLATION").is_some() {
        skip(
            TEST,
            "AWMAN_TEST_ISOLATION is set: awman uses its in-memory keychain under isolation, so \
             the host store cannot be exercised through tools/isolated-test.sh; run this test \
             directly with the native gate",
        );
        return;
    }
    let Some(inputs) = gated(
        TEST,
        "AWMAN_TEST_REGISTRY",
        &[
            "AWMAN_TEST_REGISTRY_HOST",
            "AWMAN_TEST_REGISTRY_REFERENCE",
            "AWMAN_TEST_KEYCHAIN_USERNAME",
            "AWMAN_TEST_KEYCHAIN_PASSWORD",
        ],
    ) else {
        return;
    };
    let registry = inputs["AWMAN_TEST_REGISTRY_HOST"].clone();
    let reference = inputs["AWMAN_TEST_REGISTRY_REFERENCE"].clone();
    let service = format!("awman-test-keychain-{}", std::process::id());
    let value = format!(
        "{}:{}",
        inputs["AWMAN_TEST_KEYCHAIN_USERNAME"], inputs["AWMAN_TEST_KEYCHAIN_PASSWORD"]
    );
    let entry = match DisposableKeychainEntry::store(&service, &registry, &value) {
        Ok(entry) => entry,
        Err(reason) => {
            skip(TEST, &format!("no usable host keychain: {reason}"));
            return;
        }
    };
    let report = ScenarioReport::start(TEST);
    assert!(entry.present(), "the disposable entry was stored");
    let host = RegistryHostConfig {
        insecure: truthy("AWMAN_TEST_REGISTRY_INSECURE"),
        ca_cert: optional("AWMAN_TEST_REGISTRY_CA").map(PathBuf::from),
        auth: Some(RegistryAuthSource::Keychain {
            service: service.clone(),
        }),
    };
    let temp = tempfile::tempdir().unwrap();
    let got = acquirer(&temp.path().join("state"))
        .acquire(
            &AcquireRequest {
                tag: "awman-real-keychain:latest".into(),
                source: ImageSourceSpec::Registry {
                    registry: Some(registry.clone()),
                    reference: Some(reference.clone()),
                },
                platform: OciPlatform::host_linux(),
                policy: AcquirePolicy::IfMissing,
                registries: BTreeMap::from([(registry.clone(), host)]),
            },
            &mut |_| {},
        )
        .unwrap_or_else(|e| panic!("{TEST}: keychain-authenticated pull failed: {e}"));
    assert_eq!(got.identity.source, ImageSourceKind::Registry);
    drop(entry);
    let leftover = DisposableKeychainEntry {
        service: service.clone(),
        account: registry,
    };
    let still_there = leftover.present();
    std::mem::forget(leftover);
    assert!(!still_there, "the disposable entry was removed");
    report.pass(&format!(
        "pulled {reference} with disposable keychain credentials and verified removal"
    ));
}
