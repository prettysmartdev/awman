use crate::support;

#[cfg(target_os = "linux")]
#[path = "../../boundary/registry/client.rs"]
mod registry_client;
/// Guards: f23e75a4 fixture is required and digest-verified before import.
#[test]
fn pinned_image_defaults_and_cached_import_are_preserved() {
    let s = support::sandbox("import::pinned_image_defaults_and_cached_import_are_preserved");
    let out = support::scenario(&s, "image-defaults");
    assert_eq!(
        out.lines().filter(|l| l.starts_with("IMPORT\t")).count(),
        2,
        "{out}"
    );
    assert!(out.contains("EXIT\t0"), "{out}");
    let guest = std::fs::read_to_string(s.work().join("image-defaults.stdout")).unwrap();
    support::transcript(&guest, "image-checks.sh", "image-contract-pass");
}

/// Prove the registry acquisition path reaches a real guest, then survives loss
/// of the source without network access or the builder fallback.
#[test]
#[cfg(target_os = "linux")]
fn registry_ready_boot_and_offline_cache_survive_source_deletion() {
    use awman_test_support::capability::{Docker, LocalRegistry, Offline};

    let mut s =
        support::sandbox("import::registry_ready_boot_and_offline_cache_survive_source_deletion");
    let archive = support::fixture(&s);
    let docker = s.require::<Docker>();
    s.grant(&docker);
    let registry = s.require::<LocalRegistry>();
    let host = registry.addr.to_string();
    let repository = format!(
        "guest-import/{}",
        s.root()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase()
    );
    let reference = format!("{repository}:fixture");
    let tag = format!("{host}/{reference}");
    s.command(&docker.cli)
        .args(["image", "load", "--input"])
        .arg(s.fixture("guest-docker-base"))
        .run()
        .assert_success();
    s.command(&docker.cli)
        .args(["image", "tag", "awman-test/guest:fixture", &tag])
        .run()
        .assert_success();
    s.command(&docker.cli)
        .args(["image", "push", &tag])
        .run()
        .assert_success();
    s.command(&docker.cli)
        .args(["image", "rm", &tag])
        .run()
        .assert_success();

    support::watch(&s);
    let repo = support::session::prepare(
        &s,
        &archive,
        "claude",
        "#!/bin/sh\nset -eu\nIFS= read -r prompt\nprintf 'REGISTRY_GUEST_UID=%s\\n' \"$(id -u)\"\nprintf 'REGISTRY_CONTENT=%s\\n' \"$(cat /compat/opaque/new)\"\nprintf 'REGISTRY_PROMPT=%s\\n' \"$prompt\"\n",
    );
    let global_path = s.awman_home().join("config.json");
    let mut global: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&global_path).unwrap()).unwrap();
    let builtin = &mut global["runtime"]["builtin"];
    builtin["builder"] = serde_json::json!({"enabled":false});
    builtin["imageSource"] =
        serde_json::json!({"type":"registry", "registry":host, "reference":reference});
    builtin["registries"] = serde_json::json!({host.clone(): {"insecure":true}});
    std::fs::write(global_path, serde_json::to_vec(&global).unwrap()).unwrap();
    // Any accidental builder call fails, even while the source is available.
    let docker_trap = s.install_fake(support::fake_cli_exe(), "docker");
    docker_trap.on(&[]).exit(91);
    let container_trap = s.install_fake(support::fake_cli_exe(), "container");
    container_trap.on(&[]).exit(91);
    support::session::ready(&s, support::awman_exe(), &repo);
    let before = support::session::exec(&s, support::awman_exe(), &repo, "claude", "online");
    before.assert_success();
    assert!(before.stdout_text().contains("REGISTRY_GUEST_UID=1234"));
    assert!(before.stdout_text().contains("REGISTRY_CONTENT=visible"));
    assert!(before.stdout_text().contains("REGISTRY_PROMPT=online"));

    let manifest_path = format!("/v2/{repository}/manifests/fixture");
    let accept = [(
        "Accept",
        "application/vnd.docker.distribution.manifest.v2+json",
    )];
    let manifest =
        registry_client::request(registry.addr, "GET", &manifest_path, &accept, b"").unwrap();
    assert_eq!(manifest.status, 200, "{:?}", manifest.body);
    let digest = manifest.header("docker-content-digest").unwrap();
    let deleted = registry_client::request(
        registry.addr,
        "DELETE",
        &format!("/v2/{repository}/manifests/{digest}"),
        &[],
        b"",
    )
    .unwrap();
    assert_eq!(
        deleted.status, 202,
        "registry must enable manifest deletion: {:?}",
        deleted.body
    );
    let missing =
        registry_client::request(registry.addr, "GET", &manifest_path, &accept, b"").unwrap();
    assert_eq!(
        missing.status, 404,
        "source image must actually be gone: {:?}",
        missing.body
    );

    s.require::<Offline>();
    let probe = s
        .command("/usr/bin/unshare")
        .args(["--user", "--map-root-user", "--net", "--", "/bin/true"])
        .run();
    assert!(
        probe.exit.success(),
        "BLOCKED[offline]: network namespace unavailable: {probe:?}"
    );
    let offline = || {
        s.command("/usr/bin/unshare")
            .args(["--user", "--map-root-user", "--net", "--"])
            .arg(support::awman_exe())
            .current_dir(&repo)
            .env("AWMAN_TEST_BUILTIN", "1")
    };
    let ready = offline().args(["ready", "--json"]).run();
    ready.assert_success();
    let result: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
    assert_eq!(result["ready"], true, "{result}");
    let cached = offline()
        .args([
            "exec",
            "prompt",
            "-n",
            "--agent",
            "claude",
            "cached-offline",
        ])
        .run();
    cached.assert_success();
    assert!(cached.stdout_text().contains("REGISTRY_GUEST_UID=1234"));
    assert!(cached.stdout_text().contains("REGISTRY_CONTENT=visible"));
    assert!(cached
        .stdout_text()
        .contains("REGISTRY_PROMPT=cached-offline"));
    assert!(
        docker_trap.calls().is_empty() && container_trap.calls().is_empty(),
        "registry import or cached execution reached a builder"
    );
}
