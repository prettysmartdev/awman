//! Authenticated pulls from a real private repository: credentials from
//! environment variables and from Docker's `config.json`, a wrong token and
//! no credentials refused, and no secret in any error. Nightly only.
//!
//! Needs the `registry-credentials` capability (a private file naming the
//! registry host, user and a disposable read-only token) and the repository
//! as the host resource `private-registry-reference=<repository>:<tag>`.

use super::snapshot_with;
use crate::support;
use awman::data::config::image_source::{
    ImageSourceKind, ImageSourceSpec, RegistryAuthSource, RegistryHostConfig,
};
use awman::data::oci_identity::OciPlatform;
use awman::engine::oci::{
    default_acquirer, AcquireLimits, AcquirePolicy, AcquireRequest, ImageAcquirer,
};
use awman_test_support::capability::{PublicNetwork, RegistryCredentials};
use base64::Engine as _;
use std::collections::BTreeMap;

/// Replaces `store_registry_private_normal_cases`.
#[test]
fn private_registry_accepts_credentials_and_refuses_wrong_or_missing_ones() {
    let sandbox = support::soak_sandbox(
        "soak::private_registry::private_registry_accepts_credentials_and_refuses_wrong_or_missing_ones",
    );
    let _ = sandbox.require::<PublicNetwork>();
    let credentials = sandbox.require::<RegistryCredentials>();
    let (registry, user) = (credentials.registry.clone(), credentials.username.clone());
    let token = credentials.password().to_string();
    sandbox.redact(&token);
    let reference = awman_test_support::runtime_env::get()
        .host_resource("private-registry-reference")
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "BLOCKED[registry-credentials]: name the private repository as \
                 private-registry-reference=<repository>:<tag> in AWMAN_TEST_HOST_RESOURCES"
            )
        })
        .to_string();

    let request = |auth: Option<RegistryAuthSource>| AcquireRequest {
        tag: "awman-private-probe:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some(reference.clone()),
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::from([(
            registry.clone(),
            RegistryHostConfig {
                insecure: false,
                ca_cert: None,
                auth,
            },
        )]),
    };
    let no_secret = |text: &str| assert!(!text.contains(&token), "an error exposed the token");
    let env_auth = || RegistryAuthSource::Env {
        username_var: "AWMAN_PROBE_USER".into(),
        password_var: "AWMAN_PROBE_TOKEN".into(),
    };

    // 1. Environment variables.
    let env = snapshot_with(
        &sandbox,
        &[
            ("AWMAN_PROBE_USER", user.clone()),
            ("AWMAN_PROBE_TOKEN", token.clone()),
        ],
    );
    let by_env = default_acquirer(&sandbox.tmp().join("env"), AcquireLimits::DEFAULT, &env)
        .acquire(&request(Some(env_auth())), &mut |_| {})
        .unwrap_or_else(|e| {
            no_secret(&e.to_string());
            panic!("env-credential pull failed: {e}")
        });
    assert_eq!(by_env.identity.source, ImageSourceKind::Registry);

    // 2. Docker config.json (inline `auths`, as `docker login` writes).
    let docker_dir = sandbox.tmp().join("docker-config");
    std::fs::create_dir_all(&docker_dir).expect("docker config directory");
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
    std::fs::write(
        docker_dir.join("config.json"),
        serde_json::json!({"auths": {registry.clone(): {"auth": encoded}}}).to_string(),
    )
    .expect("docker config.json");
    let env = snapshot_with(
        &sandbox,
        &[("DOCKER_CONFIG", docker_dir.to_string_lossy().into_owned())],
    );
    let by_config = default_acquirer(&sandbox.tmp().join("config"), AcquireLimits::DEFAULT, &env)
        .acquire(
            &request(Some(RegistryAuthSource::DockerConfig)),
            &mut |_| {},
        )
        .unwrap_or_else(|e| {
            no_secret(&e.to_string());
            panic!("docker-config pull failed: {e}")
        });
    assert_eq!(
        by_config.identity.manifest_digest,
        by_env.identity.manifest_digest
    );

    // 3. A wrong token and no credentials are refused, and leak nothing.
    let env = snapshot_with(
        &sandbox,
        &[
            ("AWMAN_PROBE_USER", user),
            ("AWMAN_PROBE_TOKEN", format!("{token}-wrong")),
        ],
    );
    let wrong = default_acquirer(&sandbox.tmp().join("wrong"), AcquireLimits::DEFAULT, &env)
        .acquire(&request(Some(env_auth())), &mut |_| {})
        .unwrap_err();
    let anonymous = default_acquirer(
        &sandbox.tmp().join("anon"),
        AcquireLimits::DEFAULT,
        &support::env_snapshot(&sandbox),
    )
    .acquire(&request(None), &mut |_| {})
    .unwrap_err();
    for (what, error) in [("wrong token", &wrong), ("anonymous", &anonymous)] {
        no_secret(&error.to_string());
        let detail = error.to_string().to_ascii_lowercase();
        assert!(
            [
                "authentication",
                "unauthorized",
                "forbidden",
                "denied",
                "not found"
            ]
            .iter()
            .any(|w| detail.contains(w)),
            "{what} must be refused by the registry: {error}"
        );
    }
    sandbox.finish();
}
