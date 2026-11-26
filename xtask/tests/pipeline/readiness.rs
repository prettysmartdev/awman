use super::*;
use tar::{Builder, Header};

fn sandbox(id: &'static str) -> TestSandbox {
    let mut s = TestSandbox::for_test(TestCase::component(id));
    s.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
    s
}

fn base(path: &Path, valid_layers: bool) -> String {
    let layer = Builder::new(Vec::new()).into_inner().unwrap();
    let digest = if valid_layers {
        sha(&layer)
    } else {
        "0".repeat(64)
    };
    let config = json!({"os":"linux", "architecture":"arm64",
        "rootfs":{"type":"layers", "diff_ids":[format!("sha256:{digest}")]}, "config":{}});
    let mut archive = Builder::new(Vec::new());
    for (name, bytes) in [
        ("config.json", serde_json::to_vec(&config).unwrap()),
        ("layer.tar", layer),
        (
            "manifest.json",
            br#"[{"Config":"config.json","Layers":["layer.tar"]}]"#.to_vec(),
        ),
    ] {
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        archive
            .append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    std::fs::write(path, archive.into_inner().unwrap()).unwrap();
    sha_file(path)
}

fn recipe(
    s: &TestSandbox,
    command: &str,
    base: &Path,
    hash: &str,
    out: &Path,
) -> awman_test_support::process::Output {
    let mut c = s
        .command(awman_test_support::cargo_bin!("xtask"))
        .args(["fixtures", command]);
    if command == "build" {
        c = c.arg("guest-oci-base");
    }
    c.arg("--base")
        .arg(base)
        .args(["--sha256", hash])
        .arg("--out")
        .arg(out)
        .run()
}

#[test]
fn reproduced_archives_and_provenance_refuse_changed_or_missing_publication_bytes() {
    let s = sandbox("pipeline::fixtures::reproduce_and_refuse_tampering");
    let input = s.work().join("base.tar");
    let hash = base(&input, true);
    let first = s.work().join("first");
    let second = s.work().join("second");
    recipe(&s, "build", &input, &hash, &first).assert_success();
    recipe(&s, "build", &input, &hash, &second).assert_success();
    recipe(&s, "verify-build", &input, &hash, &first).assert_success();
    for name in ["fixture-oci.tar", "fixture-docker.tar", "provenance.json"] {
        assert_eq!(
            std::fs::read(first.join(name)).unwrap(),
            std::fs::read(second.join(name)).unwrap()
        );
    }
    for name in ["fixture-oci.tar", "fixture-docker.tar", "provenance.json"] {
        let original = std::fs::read(first.join(name)).unwrap();
        std::fs::write(first.join(name), b"substituted").unwrap();
        assert!(
            !recipe(&s, "verify-build", &input, &hash, &first)
                .exit
                .success(),
            "{name}"
        );
        std::fs::remove_file(first.join(name)).unwrap();
        assert!(
            !recipe(&s, "verify-build", &input, &hash, &first)
                .exit
                .success(),
            "missing {name}"
        );
        std::fs::write(first.join(name), original).unwrap();
    }
    // A changed archive plus its self-consistent, forged sidecar still fails.
    std::fs::write(first.join("fixture-oci.tar"), b"substituted").unwrap();
    let mut provenance: Value =
        serde_json::from_slice(&std::fs::read(first.join("provenance.json")).unwrap()).unwrap();
    provenance["archives"]["fixture-oci.tar"] = sha(b"substituted").into();
    std::fs::write(
        first.join("provenance.json"),
        serde_json::to_vec(&provenance).unwrap(),
    )
    .unwrap();
    assert!(!recipe(&s, "verify-build", &input, &hash, &first)
        .exit
        .success());
}

#[test]
fn absent_invalid_or_corrupt_base_inputs_never_produce_fixtures() {
    let s = sandbox("pipeline::fixtures::refuse_bad_base");
    let input = s.work().join("base.tar");
    let out = s.work().join("out");
    let hash = base(&input, true);
    for pin in ["pending", "latest", &"a".repeat(64)] {
        assert!(!recipe(&s, "build", &input, pin, &out).exit.success());
        assert!(!out.exists());
    }
    std::fs::remove_file(&input).unwrap();
    assert!(!recipe(&s, "build", &input, &hash, &out).exit.success());
    let hash = base(&input, false);
    assert!(!recipe(&s, "build", &input, &hash, &out).exit.success());
    assert!(!out.exists());
}

#[test]
fn fixture_fetch_and_verify_require_present_digest_matching_bytes() {
    let s = sandbox("pipeline::fixtures::fetch_and_verify");
    let store = s.work().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let mut m: toml::Value =
        toml::from_str(&std::fs::read_to_string(repo_root().join("ci/gates.toml")).unwrap())
            .unwrap();
    m["previous_release"] = toml::Value::Table(Default::default());
    let fixtures = m["fixtures"].as_table_mut().unwrap();
    fixtures.retain(|id, _| id == "guest-oci-base" || id == "guest-docker-base");
    for (id, f) in fixtures {
        let f = f.as_table_mut().unwrap();
        f.remove("pending");
        f.remove("source"); // No network fallback during this test.
        f.insert("sha256".into(), sha(id.as_bytes()).into());
        std::fs::write(store.join(f["file"].as_str().unwrap()), id.as_bytes()).unwrap();
    }
    std::fs::create_dir_all(s.work().join("ci")).unwrap();
    std::fs::write(s.work().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    std::fs::write(s.work().join("ci/gates.toml"), toml::to_string(&m).unwrap()).unwrap();
    let invoke = |command| {
        s.command(awman_test_support::cargo_bin!("xtask"))
            .env("CARGO_MANIFEST_DIR", s.work().join("xtask"))
            .args(["fixtures", command, "--store"])
            .arg(&store)
            .run()
    };
    invoke("fetch").assert_success();
    invoke("verify").assert_success();
    let archive = store.join("guest-oci-base.tar");
    std::fs::write(&archive, b"changed").unwrap();
    assert!(!invoke("fetch").exit.success());
    assert!(!invoke("verify").exit.success());
    std::fs::remove_file(&archive).unwrap();
    assert!(!invoke("fetch").exit.success());
    assert!(!invoke("verify").exit.success());
}

#[test]
fn unrelated_pending_guest_pins_do_not_block_a_unit_fixture_fetch() {
    let s = sandbox("pipeline::fixtures::tier_scoped_pins");
    let mut manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(repo_root().join("ci/gates.toml")).unwrap())
            .unwrap();
    manifest["fixtures"]
        .as_table_mut()
        .unwrap()
        .retain(|id, _| id == "guest-oci-base");
    let fixture = manifest["fixtures"]["guest-oci-base"]
        .as_table_mut()
        .unwrap();
    fixture.remove("sha256");
    fixture.insert(
        "pending".into(),
        "test fixture has not been published".into(),
    );
    manifest["previous_release"] = toml::Value::Table(Default::default());
    std::fs::create_dir_all(s.work().join("ci")).unwrap();
    std::fs::write(
        s.work().join("ci/gates.toml"),
        toml::to_string(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(s.work().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    let invoke = |tier| {
        s.command(awman_test_support::cargo_bin!("xtask"))
            .env("CARGO_MANIFEST_DIR", s.work().join("xtask"))
            .args(["fixtures", "fetch", "--tier", tier, "--store"])
            .arg(s.tmp().join("store"))
            .run()
    };
    invoke("unit").assert_success();
    assert!(!invoke("guest").exit.success());
}
