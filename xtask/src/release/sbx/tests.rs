use super::*;
fn cases(out: &Path) -> Qualification {
    let q = Qualification {
        cli_version: "0.54.1".into(),
        operator_label: "isolated-test-account".into(),
        inputs_sha256: inputs_digest("0.54.1", "isolated-test-account").unwrap(),
        cli_sha256: "c".repeat(64),
    };
    allocate(out, out).unwrap();
    let owned: Value = read_json(&out.join("sbx-owned.json")).unwrap();
    for (agent, name, disposition, kit) in SBX_CASES {
        super::super::util::write_json(&out.join(format!("sbx-smoke.{agent}.json")), &json!({
                "schema":1,"test":format!("awman-cli::artifact sbx_smoke::{name}"),"agent":agent,"disposition":disposition,
                "workload":if *disposition == "owned-workload" { Value::Null } else { json!(agent) },"kit_handle":kit,"kit_digest":"a".repeat(64),"sandbox":owned["cases"].as_array().unwrap().iter().find(|c| c["agent"] == *agent).unwrap()["sandbox"],
                "sbx_cli":q.cli_version,"inspect_sha256":null,"kits":null,"ready_exit":0,"task_exit":0,"output_bytes":8,"removed":true
            })).unwrap();
    }
    q
}
#[test]
fn exact_cases_bind_cli_identity_kit_and_successful_cleanup() {
    let out = tempfile::tempdir().unwrap();
    let q = cases(out.path());
    let bound = collect(out.path(), &q).unwrap();
    verify(out.path(), &bound).unwrap();
    for (key, value) in [
        ("test", json!("another test")),
        ("agent", json!("cline")),
        ("disposition", json!("owned-workload")),
        ("workload", Value::Null),
        ("kit_handle", json!("awman-codex-mixin")),
        ("kit_digest", json!("stale")),
        ("sandbox", json!("awman-unowned-claude")),
        ("sbx_cli", json!("0.52.0")),
        ("ready_exit", json!(1)),
        ("task_exit", json!(1)),
        ("output_bytes", json!(0)),
        ("removed", json!(false)),
    ] {
        let file = out.path().join("sbx-smoke.claude.json");
        let original = std::fs::read(&file).unwrap();
        let mut v: Value = serde_json::from_slice(&original).unwrap();
        v[key] = value;
        super::super::util::write_json(&file, &v).unwrap();
        assert!(verify(out.path(), &bound).is_err(), "tampered digest {key}");
        // Even a rewritten case hash cannot turn product failure into a pass.
        assert!(collect(out.path(), &q).is_err(), "invalid case {key}");
        std::fs::write(file, original).unwrap();
    }
    let mut tampered = bound.clone();
    tampered["cases"].as_array_mut().unwrap().pop();
    assert!(verify(out.path(), &tampered).is_err());
    tampered = bound;
    tampered["operator_label"] = json!("personal-account");
    assert!(verify(out.path(), &tampered).is_err());
}
#[test]
fn preflight_requires_sign_in_negative_control_and_both_network_destinations() {
    let probe = |fault: &str| {
        qualify("isolated", &"a".repeat(64), |request| {
            let (name, ok, output) = match request {
                Probe::Version => ("version", true, b"sbx 0.54.1".to_vec()),
                Probe::SignedIn => ("sign-in", true, vec![]),
                Probe::EmptyHome => ("negative", false, vec![]),
                Probe::EmptyHomeSecrets => ("negative-secrets", true, b"[]".to_vec()),
                Probe::DedicatedSecrets => ("dedicated-secrets", true, b"[]".to_vec()),
                Probe::Network(host) => (host, true, vec![]),
            };
            Ok((if name == fault { !ok } else { ok }, output))
        })
    };
    probe("").unwrap();
    for (fault, reason) in [
        ("version", "sbx-cli-incompatible"),
        ("sign-in", "sbx-not-signed-in"),
        ("negative", "sbx-identity-not-isolated"),
        ("dedicated-secrets", "sbx-identity-not-isolated"),
        ("docker.io", "sbx-network"),
        ("api.anthropic.com", "sbx-network"),
    ] {
        let error = probe(fault).unwrap_err();
        let stop = error.downcast_ref::<Stop>().unwrap();
        assert_eq!(stop.code, reason);
        assert_eq!(stop.exit_code(), 2);
    }
    let timed = qualify("isolated", &"a".repeat(64), |_| Err(blocked("timeout"))).unwrap_err();
    assert_eq!(timed.downcast_ref::<Stop>().unwrap().code, "timeout");
}

#[test]
fn isolated_home_refuses_personal_home_ancestors_and_aliases() {
    let root = tempfile::tempdir().unwrap();
    let personal = root.path().join("personal");
    let dedicated = root.path().join("dedicated");
    std::fs::create_dir(&personal).unwrap();
    std::fs::create_dir(&dedicated).unwrap();
    for home in [&personal, &root.path().to_path_buf()] {
        assert!(isolated_home(home, std::slice::from_ref(&personal)).is_err());
    }
    isolated_home(&dedicated, std::slice::from_ref(&personal)).unwrap();
    #[cfg(unix)]
    {
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&personal, &alias).unwrap();
        assert!(isolated_home(&alias, &[personal]).is_err());
    }
}

#[test]
fn secret_negative_controls_reject_shared_global_stores_and_unknown_shapes() {
    for fault in [
        "empty-shared",
        "dedicated-shared",
        "unknown",
        "dedicated-error",
    ] {
        let error = qualify("label", &"a".repeat(64), |request| {
            Ok(match request {
                Probe::Version => (true, b"0.54.1".to_vec()),
                Probe::SignedIn | Probe::Network(_) => (true, vec![]),
                Probe::EmptyHome => (false, vec![]),
                Probe::EmptyHomeSecrets if fault == "empty-shared" => {
                    (true, b"[{\"service\":\"anthropic\"}]".to_vec())
                }
                Probe::DedicatedSecrets if fault == "dedicated-shared" => {
                    (true, b"[{\"service\":\"anthropic\"}]".to_vec())
                }
                Probe::DedicatedSecrets if fault == "unknown" => (true, b"{}".to_vec()),
                Probe::DedicatedSecrets if fault == "dedicated-error" => (false, vec![]),
                _ => (true, b"[]".to_vec()),
            })
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Stop>().unwrap().code,
            "sbx-identity-not-isolated"
        );
    }
}

#[test]
fn qualification_inputs_and_minimum_cli_are_part_of_reuse_identity() {
    assert_eq!(version("sbx version v0.54.1"), Some("0.54.1".into()));
    assert_eq!(version("0.53.0"), Some("0.53.0".into()));
    for raw in ["0.52.9", "nonsense", "0.54", ""] {
        assert_eq!(version(raw), None);
    }
    assert_ne!(
        inputs_digest("0.54.1", "isolated").unwrap(),
        inputs_digest("0.54.2", "isolated").unwrap()
    );
    assert_ne!(
        inputs_digest("0.54.1", "isolated").unwrap(),
        inputs_digest("0.54.1", "personal").unwrap()
    );
    let ids = native_tests(SBX_NATIVE).unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids[0].contains("claude_published_workload"));
    assert!(ids[1].contains("opencode_owned_workload"));
}
#[test]
fn isolated_input_files_reject_shared_permissions_and_symlinks() {
    let out = tempfile::tempdir().unwrap();
    let key = out.path().join("key");
    std::fs::write(&key, "test").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        private(&key, false, 0o600).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private(&key, false, 0o600).is_err());
        let link = out.path().join("link");
        std::os::unix::fs::symlink(&key, &link).unwrap();
        assert!(private(&link, false, 0o600).is_err());
    }
    assert!(private(Path::new("relative"), false, 0o600).is_err());
}

#[test]
fn cleanup_requires_exact_owned_names_and_a_readable_listing() {
    for v in [
        json!([{"name":"awman-owned-claude"}]),
        json!({"Sandboxes":[{"Name":"awman-owned-claude"}]}),
    ] {
        assert!(contains_name(&v, "awman-owned-claude").unwrap());
        assert!(!contains_name(&v, "awman-owned").unwrap());
    }
    assert!(!contains_name(&json!([]), "awman-owned-claude").unwrap());
    for v in [
        json!(null),
        json!({}),
        json!([{"state":"running"}]),
        json!([{"name":"valid"},{"state":"unknown"}]),
        json!([{"name":"a","Name":"b"}]),
        json!({"sandboxes":[],"Sandboxes":[{"name":"owned"}]}),
    ] {
        assert!(contains_name(&v, "awman-owned-claude").is_err());
    }
    let root = tempfile::tempdir().unwrap();
    allocate(root.path(), root.path()).unwrap();
    let mut ledger: Value = read_json(&root.path().join("sbx-owned.json")).unwrap();
    owned_cases(&ledger).unwrap();
    ledger["cases"][0]["sandbox"] = json!("awman-foreign-claude");
    assert!(owned_cases(&ledger).is_err());
    ledger["cases"][0]["workspace"] = json!("/some/other/checkout");
    assert!(owned_cases(&ledger).is_err());
}

#[test]
fn release_probe_rejects_nonempty_or_unreadable_global_store() {
    let run = |shared: bool, unreadable: bool| {
        qualify("test", &"a".repeat(64), |request| {
            Ok(match request {
                Probe::Version => (true, b"0.54.1".to_vec()),
                Probe::SignedIn | Probe::Network(_) => (true, vec![]),
                Probe::EmptyHome => (false, vec![]),
                Probe::EmptyHomeSecrets => (
                    true,
                    if shared {
                        b"[{}]".to_vec()
                    } else {
                        b"[]".to_vec()
                    },
                ),
                Probe::DedicatedSecrets => (
                    true,
                    if unreadable {
                        b"{}".to_vec()
                    } else {
                        b"[{\"service\":\"anthropic\"}]".to_vec()
                    },
                ),
            })
        })
    };
    assert!(run(false, false).is_err());
    assert!(run(true, false).is_err());
    assert!(run(false, true).is_err());
}
