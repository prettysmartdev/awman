use super::*;
use awman_test_support::{TestCase, TestSandbox};

fn response(step: &str, workspace: &Path) -> Vec<u8> {
    match step {
        "version" => b"sbx version 0.54.1".to_vec(),
        "session" | "cleanup" => b"[]".to_vec(),
        "inspect" => br#"{"name":"awman-validate-test"}"#.to_vec(),
        "exec" | "run" => {
            fs::copy(
                workspace.join(".host-probe"),
                workspace.join(".guest-probe"),
            )
            .unwrap();
            vec![]
        }
        _ => vec![],
    }
}

#[test]
fn host_selection_runs_mac_only_unless_linux_was_requested_and_preserves_failures() {
    for (destination, expected) in [
        (None, vec![ValidationHost::Mac]),
        (
            Some("user@host"),
            vec![ValidationHost::Mac, ValidationHost::Linux("user@host")],
        ),
    ] {
        let mut visited = Vec::new();
        run_selected_hosts(destination, |host| {
            visited.push(host);
            Ok(())
        })
        .unwrap();
        assert_eq!(visited, expected);
    }
    let mut visited = Vec::new();
    assert!(run_selected_hosts(Some("user@host"), |host| {
        visited.push(host);
        anyhow::bail!("Mac failed")
    })
    .is_err());
    assert_eq!(
        visited,
        vec![ValidationHost::Mac],
        "Mac failure must not attempt SSH"
    );
    assert!(
        run_selected_hosts(Some("user@host"), |host| {
            ensure!(host == ValidationHost::Mac, "Linux failed");
            Ok(())
        })
        .is_err(),
        "a Linux failure must fail the combined run"
    );
}

#[test]
fn logged_in_smoke_uses_no_credential_setup_and_checks_both_guest_entry_points() {
    let s = TestSandbox::for_test(TestCase::component("sbx_validation::tests::logged_in_smoke_uses_no_credential_setup_and_checks_both_guest_entry_points"));
    let mut calls = Vec::new();
    let version = smoke("awman-validate-test", &s.work(), "nonce", |step, args| {
        calls.push((
            step.to_owned(),
            args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        ));
        if step == "run" {
            assert!(
                !s.work().join(".guest-probe").exists(),
                "exec witness must not satisfy run"
            );
        }
        Ok(response(step, &s.work()))
    })
    .unwrap();
    assert_eq!(version, "0.54.1");
    assert_eq!(
        calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        STEPS
    );
    let create = &calls.iter().find(|c| c.0 == "create").unwrap().1;
    assert_eq!(
        create,
        &[
            "--no-input",
            "create",
            "--name",
            "awman-validate-test",
            "shell",
            s.work().to_str().unwrap()
        ]
    );
    assert!(calls.iter().all(|(_, args)| !args
        .iter()
        .any(|a| ["secret", "login", "--cloud"].contains(&a.as_str()))));
    let c = command(Path::new("/installed/sbx"), &s.work(), &["--version"]);
    assert_eq!(c.get_current_dir(), Some(s.work().as_path()));
    assert_eq!(
        c.get_envs().count(),
        0,
        "do not override the logged-in session with test inputs"
    );
    s.finish();
}

#[test]
fn every_partial_launch_failure_still_removes_only_the_owned_sandbox() {
    let s = TestSandbox::for_test(TestCase::component(
        "sbx_validation::tests::every_partial_launch_failure_still_removes_only_the_owned_sandbox",
    ));
    for fault in ["create", "inspect", "exec", "run", "stop", "restart"] {
        let workspace = s.work().join(fault);
        fs::create_dir(&workspace).unwrap();
        let mut calls = Vec::new();
        let error = smoke("awman-validate-test", &workspace, "nonce", |step, args| {
            calls.push(step.to_owned());
            if step == "remove" {
                assert_eq!(
                    args,
                    &["--no-input", "rm", "--force", "awman-validate-test"]
                );
            }
            ensure!(step != fault, "injected {fault} failure");
            Ok(response(step, &workspace))
        })
        .unwrap_err();
        assert!(error.to_string().contains(fault), "{error}");
        assert_eq!(&calls[calls.len() - 2..], &["remove", "cleanup"]);
    }
    s.finish();
}

#[test]
fn bad_session_version_and_name_collision_never_mutate_existing_sandboxes() {
    let s = TestSandbox::for_test(TestCase::component("sbx_validation::tests::bad_session_version_and_name_collision_never_mutate_existing_sandboxes"));
    for (version, listing) in [
        ("0.53.0", "[]"),
        ("unknown", "[]"),
        ("0.54.1", "{}"),
        ("0.54.1", r#"[{"name":"awman-validate-test"}]"#),
        ("0.54.1", r#"[{"name":"other"},{"unknown":"row"}]"#),
    ] {
        assert!(smoke("awman-validate-test", &s.work(), "nonce", |step, _| {
            Ok(match step {
                "version" => version.as_bytes().to_vec(),
                "session" => listing.as_bytes().to_vec(),
                _ => panic!("must refuse before {step}"),
            })
        })
        .is_err());
    }
    s.finish();
}

#[test]
fn failed_removal_partial_listing_and_leaks_never_count_as_cleaned_up() {
    let s = TestSandbox::for_test(TestCase::component(
        "sbx_validation::tests::failed_removal_partial_listing_and_leaks_never_count_as_cleaned_up",
    ));
    for (i, (remove_ok, listing)) in [
        (false, "[]"),
        (true, "{}"),
        (true, r#"[{"name":"other"},{"unknown":"row"}]"#),
        (true, r#"[{"name":"awman-validate-test"}]"#),
    ]
    .iter()
    .enumerate()
    {
        let workspace = s.work().join(format!("attempt-{i}"));
        fs::create_dir(&workspace).unwrap();
        assert!(
            smoke("awman-validate-test", &workspace, "nonce", |step, _| {
                ensure!(step != "remove" || *remove_ok, "remove failed");
                if step == "cleanup" {
                    return Ok(listing.as_bytes().to_vec());
                }
                Ok(response(step, &workspace))
            })
            .is_err()
        );
    }
    s.finish();
}

#[test]
fn passing_exit_without_guest_witness_is_a_failure_and_still_cleans_up() {
    let s = TestSandbox::for_test(TestCase::component("sbx_validation::tests::passing_exit_without_guest_witness_is_a_failure_and_still_cleans_up"));
    let mut removed = false;
    assert!(smoke("awman-validate-test", &s.work(), "nonce", |step, _| {
        removed |= step == "remove";
        if step == "exec" {
            return Ok(vec![]);
        }
        Ok(response(step, &s.work()))
    })
    .is_err());
    assert!(removed);
    s.finish();
}

#[test]
fn retained_report_rejects_missing_failed_or_modified_evidence() {
    let s = TestSandbox::for_test(TestCase::component(
        "sbx_validation::tests::retained_report_rejects_missing_failed_or_modified_evidence",
    ));
    let workspace = s.work().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let mut steps = BTreeMap::new();
    let version = smoke("awman-validate-test", &workspace, "nonce", |step, _| {
        let bytes = response(step, &workspace);
        fs::write(s.work().join(format!("{step}.stdout")), &bytes)?;
        fs::write(s.work().join(format!("{step}.stderr")), b"")?;
        steps.insert(step.into(), 0);
        Ok(bytes)
    })
    .unwrap();
    let mut proof = Proof {
        schema: 2,
        outcome: Outcome::Passed,
        target: TARGET.into(),
        sandbox: "awman-validate-test".into(),
        linux_host: None,
        cli_version: Some(version),
        cli_sha256: Some("a".repeat(64)),
        steps,
        files: files(&s.work()).unwrap(),
        error: None,
    };
    proof.verify(&s.work()).unwrap();
    proof.steps.remove("run");
    assert!(proof.verify(&s.work()).is_err());
    proof.steps.insert("run".into(), 1);
    assert!(proof.verify(&s.work()).is_err());
    proof.steps.insert("run".into(), 0);
    fs::write(workspace.join(".guest-probe"), b"fake").unwrap();
    assert!(proof.verify(&s.work()).is_err());
    proof.files = files(&s.work()).unwrap();
    assert!(proof.verify(&s.work()).is_err());
    fs::copy(
        workspace.join(".host-probe"),
        workspace.join(".guest-probe"),
    )
    .unwrap();
    fs::write(
        s.work().join("cleanup.stdout"),
        br#"[{"name":"awman-validate-test"}]"#,
    )
    .unwrap();
    proof.files = files(&s.work()).unwrap();
    assert!(proof.verify(&s.work()).is_err());
    fs::write(s.work().join("cleanup.stdout"), b"[]").unwrap();
    fs::remove_file(s.work().join("run.stdout")).unwrap();
    proof.files = files(&s.work()).unwrap();
    assert!(proof.verify(&s.work()).is_err());
    s.finish();
}
