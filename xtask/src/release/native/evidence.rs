//! Reconcile native summaries with the raw execution and the attested kit.
use super::{Kit, NativeJob};
use crate::release::{
    error::{Step, Stop},
    util::{read_json, sha256_file},
};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};

pub(super) fn verify(job: &NativeJob, path: &Path) -> Step {
    let checked = || -> anyhow::Result<()> {
        use anyhow::{ensure, Context};
        let r: Value = read_json(path)?;
        let kit_path = job.kit_dir.join("kit.json");
        ensure!(
            sha256_file(&kit_path)? == job.kit_sha256,
            "kit metadata changed"
        );
        let kit: Kit = read_json(&kit_path)?;
        ensure!(
            kit.commit == job.commit
                && kit.tree == job.tree
                && kit.manifest_sha256 == job.manifest_sha256
                && kit.plan_sha256 == job.plan_sha256
                && kit.policy_sha256 == job.policy_sha256,
            "kit source or policy bindings differ"
        );
        for (key, expected) in [
            ("manifest_sha256", &job.manifest_sha256),
            ("plan_sha256", &job.plan_sha256),
            ("policy_sha256", &job.policy_sha256),
        ] {
            ensure!(
                r[key].as_str() == Some(expected),
                "{key} differs from pinned plan"
            );
        }
        ensure!(r["command_exit"] == 0, "native command did not succeed");
        ensure!(
            r["reason"].is_null() && r["reason_code"].is_null(),
            "passed result has a failure reason"
        );
        ensure!(
            r["fixtures"] == kit.fixtures,
            "fixture bindings differ from kit"
        );
        ensure!(
            r["previous_release"] == serde_json::to_value(&kit.previous_release)?,
            "predecessor differs from kit"
        );
        let capabilities = job.kit_dir.join("capabilities.json");
        let env_sha = sha256_file(&capabilities)?;
        ensure!(
            kit.files.get("capabilities.json") == Some(&env_sha),
            "unbound environment contract"
        );
        ensure!(
            r["env_contract_sha256"] == env_sha,
            "environment contract differs from kit"
        );
        let out = path.parent().context("native evidence directory")?;
        for (name, field) in [
            ("expected.json", "expected_inventory_sha256"),
            ("junit.xml", "junit_sha256"),
            ("runner.log", "runner_log_sha256"),
        ] {
            ensure!(
                r[field] == sha256_file(&out.join(name))?,
                "raw {name} digest mismatch"
            );
        }
        if job.plan.id == crate::manifest::release_policy::SBX_NATIVE {
            super::super::sbx::verify(out, &r["sbx"])?;
        }
        if [
            crate::manifest::release_policy::APPLE_NATIVE,
            crate::manifest::release_policy::DOCKER_NATIVE,
        ]
        .contains(&job.plan.id.as_str())
        {
            runtime(job, out, &r)?;
        }
        let expected: Value = read_json(&out.join("expected.json"))?;
        let inventory = crate::evidence::actions::expected(&expected)?;
        let want: BTreeSet<_> = job.plan.tests.iter().cloned().collect();
        ensure!(inventory == want, "raw inventory differs from plan");
        let compiled: Value = read_json(&job.kit_dir.join("expected.json"))?;
        ensure!(
            crate::evidence::actions::expected(&compiled)? == want,
            "attested kit inventory differs from plan"
        );
        let records =
            crate::evidence::junit::parse(&std::fs::read_to_string(out.join("junit.xml"))?)?;
        ensure!(
            records
                .iter()
                .map(|r| r.test.clone())
                .collect::<BTreeSet<_>>()
                == want,
            "raw JUnit inventory differs from plan"
        );
        let summary = r["tests"].as_array().context("result tests")?;
        for record in &records {
            ensure!(
                record.outcome == crate::evidence::schema::Outcome::Passed,
                "raw JUnit contains a non-pass"
            );
            let test = summary
                .iter()
                .find(|t| t["id"] == record.test)
                .context("missing summary result")?;
            ensure!(
                test["duration_ms"].as_u64() == Some(record.duration_ms),
                "result duration differs from raw JUnit"
            );
        }
        let time = |key: &str| -> anyhow::Result<chrono::DateTime<chrono::FixedOffset>> {
            Ok(chrono::DateTime::parse_from_rfc3339(
                r[key].as_str().context("missing native timestamp")?,
            )?)
        };
        ensure!(
            time("started")? <= time("finished")?
                && (time("finished")? - time("started")?).num_seconds()
                    <= job.plan.budget_seconds as i64,
            "native timestamps are reversed"
        );
        let (os, arch) = if job.plan.triple.contains("apple") {
            ("macos", "aarch64")
        } else {
            ("linux", "x86_64")
        };
        ensure!(
            r["host"]["os"] == os
                && r["host"]["arch"] == arch
                && r["host"]["os_build"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
            "native host identity differs from target"
        );
        Ok(())
    };
    checked().map_err(|e| Stop::new("native-result-invalid", format!("{}: {e:#}", job.plan.id)))
}

/// Verify the production runtime journey and owned cleanup, in addition to JUnit.
fn runtime(job: &NativeJob, out: &Path, r: &Value) -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    let apple = job.plan.id == crate::manifest::release_policy::APPLE_NATIVE;
    let kind = if apple { "apple" } else { "docker" };
    let service = &r["service"];
    ensure!(
        r["host"]["hostname_sha256"]
            .as_str()
            .is_some_and(crate::release::util::is_sha256),
        "missing runtime host identity"
    );
    let caps: Value = read_json(&job.kit_dir.join("capabilities.json"))?;
    ensure!(
        caps["grants"]
            == serde_json::json!(crate::manifest::release_policy::native_grants(
                &job.plan.id
            )?),
        "wrong native service grants"
    );
    ensure!(
        service["kind"] == if apple { "apple-containers" } else { "docker" },
        "wrong runtime service"
    );
    for key in [
        "cli_path",
        "cli_version",
        if apple {
            "service_version"
        } else {
            "server_version"
        },
    ] {
        ensure!(
            service[key].as_str().is_some_and(|s| !s.is_empty()),
            "missing service identity {key}"
        );
    }
    ensure!(
        service["cli_sha256"]
            .as_str()
            .is_some_and(crate::release::util::is_sha256),
        "invalid CLI digest"
    );
    for (name, key) in [
        ("runtime-owned.json", "runtime_owned_sha256"),
        ("runtime-reconcile.json", "runtime_reconcile_sha256"),
        ("runtime-smoke.json", "runtime_smoke_sha256"),
    ] {
        ensure!(r[key] == sha256_file(&out.join(name))?, "unbound {name}");
    }
    let owned: Value = read_json(&out.join("runtime-owned.json"))?;
    let audit: Value = read_json(&out.join("runtime-reconcile.json"))?;
    let smoke: Value = read_json(&out.join("runtime-smoke.json"))?;
    ensure!(
        owned["schema"] == 1
            && owned["armed"] == true
            && owned["runtime"] == kind
            && owned["cli_path"] == service["cli_path"]
            && owned["cli_sha256"] == service["cli_sha256"]
            && owned["engine"] == service["engine"],
        "ownership/service binding differs"
    );
    let token = owned["owner"]
        .as_str()
        .and_then(|s| s.strip_prefix("awr-"))
        .context("runtime owner")?;
    ensure!(
        token.len() == 16
            && token
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid runtime owner"
    );
    ensure!(
        owned["project"] == format!("awsmoke-{token}"),
        "project differs from reservation"
    );
    ensure!(
        audit["schema"] == 1
            && audit["owner"] == owned["owner"]
            && audit["leaked"] == 0
            && audit["remaining"] == 0
            && audit["verified"] == true
            && audit["removal_errors"]
                .as_array()
                .is_some_and(Vec::is_empty),
        "cleanup incomplete or leaked objects"
    );
    ensure!(
        smoke["schema"] == 1
            && smoke["runtime"] == kind
            && smoke["owner"] == owned["owner"]
            && smoke["project"] == owned["project"]
            && smoke["artifact_sha256"] == job.asset_sha256
            && smoke["ready_exit"] == 0
            && smoke["task_exit"] == 37
            && smoke["guest_os"] == "Linux"
            && smoke["prompt_observed"] == true
            && smoke["stderr_observed"] == true
            && smoke["removed"] == true
            && smoke["verified_absent"] == true,
        "incomplete runtime command/cleanup proof"
    );
    ensure!(
        smoke["containers"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "no running container observed"
    );
    // These are paths on the admitted Unix host, not on the controller running
    // the verifier (the fault world also runs in Windows CI).
    let root = owned["root"].as_str().context("private runtime root")?;
    ensure!(
        root.starts_with('/')
            && root.split('/').count() > 3
            && !root.split('/').any(|c| c == ".." || c == "."),
        "invalid private root"
    );
    let mounts = smoke["mount_sources"]
        .as_array()
        .context("missing mount audit")?;
    ensure!(!mounts.is_empty(), "empty mount audit");
    for mount in mounts {
        let p = mount.as_str().context("mount source")?;
        ensure!(
            p.starts_with(&format!("{}/", root.trim_end_matches('/')))
                && !p.split('/').any(|c| c == ".." || c == "."),
            "mount outside private runtime root"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::{native, plan::Plan, util::write_json};

    #[test]
    fn native_pass_binds_metadata_and_complete_raw_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let plan: Value = serde_json::from_slice(include_bytes!(
            "../../../tests/release_contract/fixtures/release-plan.json"
        ))
        .unwrap();
        let parsed: Plan = serde_json::from_value(plan.clone()).unwrap();
        let n = parsed
            .native
            .iter()
            .find(|n| n.host == "ssh")
            .unwrap()
            .clone();
        let spec = plan["native"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == n.id)
            .unwrap();
        let archive = tmp.path().join("kit.tar");
        std::fs::write(
            &archive,
            crate::release::forge_dir::fake_kit(spec, &plan, "R", "T"),
        )
        .unwrap();
        let kit_dir = tmp.path().join("kit");
        native::safe_extract(&archive, &kit_dir).unwrap();
        native::check_kit(&kit_dir, &n, "R").unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::copy(kit_dir.join("expected.json"), out.join("expected.json")).unwrap();
        let (class, name) = n.tests[0].split_once(' ').unwrap();
        std::fs::write(out.join("junit.xml"),format!("<testsuite><testcase classname=\"{class}\" name=\"{name}\" time=\"0.001\"/></testsuite>")).unwrap();
        std::fs::write(out.join("runner.log"), "executed").unwrap();
        let kit: Value = read_json(&kit_dir.join("kit.json")).unwrap();
        let job = NativeJob {
            plan: n.clone(),
            commit: "R".into(),
            tree: "T".into(),
            asset: tmp.path().join("asset"),
            asset_sha256: "a".repeat(64),
            kit_sha256: sha256_file(&kit_dir.join("kit.json")).unwrap(),
            kit_tar: archive.clone(),
            kit_dir: kit_dir.clone(),
            manifest_sha256: parsed.manifest_sha256,
            plan_sha256: kit["plan_sha256"].as_str().unwrap().into(),
            policy_sha256: parsed.policy_sha256,
            downloads: vec![],
            out_dir: out.clone(),
            deadline_seconds: 600,
        };
        let result = serde_json::json!({"schema":1,"native_id":n.id,"commit":"R","tree":"T",
            "artifact_sha256":job.asset_sha256,"kit_sha256":job.kit_sha256,"outcome":"passed","reason":null,"reason_code":null,
            "manifest_sha256":job.manifest_sha256,"plan_sha256":job.plan_sha256,"policy_sha256":job.policy_sha256,
            "tests":[{"id":n.tests[0],"outcome":"passed","duration_ms":1}],"command_exit":0,
            "host":{"os":"linux","arch":"x86_64","os_build":"test"},"started":"2026-11-25T01:00:00Z","finished":"2026-11-25T01:00:01Z",
            "fixtures":kit["fixtures"],"previous_release":kit["previous_release"],
            "env_contract_sha256":sha256_file(&kit_dir.join("capabilities.json")).unwrap(),
            "expected_inventory_sha256":sha256_file(&out.join("expected.json")).unwrap(),
            "junit_sha256":sha256_file(&out.join("junit.xml")).unwrap(),"runner_log_sha256":sha256_file(&out.join("runner.log")).unwrap()});
        let path = out.join("result.json");
        write_json(&path, &result).unwrap();
        native::verify_result(&job, &path, 0).unwrap();
        for (key, value) in [
            ("kit_sha256", Value::String(sha256_file(&archive).unwrap())),
            ("plan_sha256", Value::String("c".repeat(64))),
            ("fixtures", serde_json::json!([])),
            ("command_exit", serde_json::json!(1)),
            ("junit_sha256", Value::Null),
        ] {
            let mut bad = result.clone();
            bad[key] = value;
            write_json(&path, &bad).unwrap();
            assert_eq!(
                native::verify_result(&job, &path, 0).unwrap_err().code,
                "native-result-invalid",
                "{key}"
            );
        }
        write_json(&path, &result).unwrap();
        std::fs::write(out.join("runner.log"), "tampered").unwrap();
        assert!(native::verify_result(&job, &path, 0).is_err());
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::release::{plan::Plan, util::write_json};
    use awman_test_support::{TestCase, TestSandbox};
    #[test]
    fn runtime_pass_requires_bound_service_journey_mounts_and_cleanup() {
        let s = TestSandbox::for_test(TestCase::component("release::native::evidence::runtime_tests::runtime_pass_requires_bound_service_journey_mounts_and_cleanup"));
        let plan: Plan = serde_json::from_slice(include_bytes!(
            "../../../tests/release_contract/fixtures/release-plan.json"
        ))
        .unwrap();
        for id in [
            crate::manifest::release_policy::APPLE_NATIVE,
            crate::manifest::release_policy::DOCKER_NATIVE,
        ] {
            let out = s.work().join(id);
            std::fs::create_dir_all(&out).unwrap();
            write_json(&out.join("capabilities.json"), &serde_json::json!({"grants":crate::manifest::release_policy::native_grants(id).unwrap()})).unwrap();
            let job = NativeJob {
                plan: plan.native(id).unwrap().clone(),
                commit: "R".into(),
                tree: "T".into(),
                asset: out.join("asset"),
                asset_sha256: "a".repeat(64),
                kit_tar: out.join("kit.tar"),
                kit_sha256: "b".repeat(64),
                manifest_sha256: plan.manifest_sha256.clone(),
                plan_sha256: "c".repeat(64),
                policy_sha256: plan.policy_sha256.clone(),
                kit_dir: out.clone(),
                downloads: vec![],
                out_dir: out.clone(),
                deadline_seconds: 780,
            };
            let mut good = serde_json::json!({});
            crate::release::runtime_smoke::fake::native(&job, &mut good).unwrap();
            runtime(&job, &out, &good).unwrap();
            for (file, field, key, bad) in [
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "task_exit",
                    serde_json::json!(0),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "artifact_sha256",
                    serde_json::json!("stale"),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "mount_sources",
                    serde_json::json!(["/workspace"]),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "containers",
                    serde_json::json!([]),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "guest_os",
                    serde_json::json!("Darwin"),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "prompt_observed",
                    serde_json::json!(false),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "stderr_observed",
                    serde_json::json!(false),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "removed",
                    serde_json::json!(false),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "verified_absent",
                    serde_json::json!(false),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "ready_exit",
                    serde_json::json!(1),
                ),
                (
                    "runtime-smoke.json",
                    "runtime_smoke_sha256",
                    "project",
                    serde_json::json!("foreign"),
                ),
                (
                    "runtime-reconcile.json",
                    "runtime_reconcile_sha256",
                    "leaked",
                    serde_json::json!(1),
                ),
                (
                    "runtime-reconcile.json",
                    "runtime_reconcile_sha256",
                    "verified",
                    serde_json::json!(false),
                ),
                (
                    "runtime-reconcile.json",
                    "runtime_reconcile_sha256",
                    "removal_errors",
                    serde_json::json!(["docker rm returned 17"]),
                ),
                (
                    "runtime-owned.json",
                    "runtime_owned_sha256",
                    "armed",
                    serde_json::json!(false),
                ),
                (
                    "runtime-owned.json",
                    "runtime_owned_sha256",
                    "cli_sha256",
                    serde_json::json!("differing-cli"),
                ),
            ] {
                let saved = std::fs::read(out.join(file)).unwrap();
                let mut value: Value = serde_json::from_slice(&saved).unwrap();
                value[key] = bad;
                write_json(&out.join(file), &value).unwrap();
                let mut forged = good.clone();
                forged[field] = serde_json::json!(sha256_file(&out.join(file)).unwrap());
                assert!(
                    runtime(&job, &out, &forged).is_err(),
                    "{id}: accepted forged {key}"
                );
                std::fs::write(out.join(file), saved).unwrap();
            }
            let mut bad = good.clone();
            bad["runtime_owned_sha256"] = Value::Null;
            assert!(runtime(&job, &out, &bad).is_err());
            bad = good.clone();
            bad["host"]["hostname_sha256"] = Value::Null;
            assert!(runtime(&job, &out, &bad).is_err());
        }
    }
}
