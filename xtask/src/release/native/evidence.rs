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
            time("started")? <= time("finished")?,
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
