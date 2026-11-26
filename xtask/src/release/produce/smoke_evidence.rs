//! Consume the raw files written by `test run --evidence-dir`.
use super::*;
use std::collections::BTreeSet;

pub(super) fn read(dir: &Path, a: &AssetPlan, commit: &str, tree: &str) -> Result<Value> {
    let mut expected = Vec::new();
    let mut results = Vec::new();
    let mut raw = Vec::new();
    for suite in &a.smoke_suites {
        let out = dir.join(suite);
        let run_path = out.join("run.json");
        let run: Value = read_json(&run_path)?;
        ensure!(
            run["schema"] == 1 && run["runner"] == "nextest" && run["build"] == a.build,
            "{suite}: wrong raw run schema/runner/build"
        );
        ensure!(
            run["failed_suites"] == json!([])
                && run["source"]["commit"] == commit
                && run["source"]["tree"] == tree
                && run["source"]["clean"] == true,
            "{suite}: failed run or source mismatch"
        );
        let invocations = run["invocations"]
            .as_array()
            .context("missing smoke invocations")?;
        ensure!(
            invocations.len() == 1,
            "{suite}: expected exactly one invocation"
        );
        let inv = &invocations[0];
        ensure!(
            inv["exit"] == 0 && inv["suites"] == json!([suite]),
            "{suite}: unsuccessful or wrong invocation"
        );
        let name = inv["expected"]
            .as_str()
            .context("missing compiled smoke inventory")?;
        let path = crate::evidence::actions::file_in(&out, name)?;
        let inventory: Value = read_json(&path)?;
        let want = crate::evidence::actions::expected(&inventory)?;
        let junit = out.join(format!("junit-artifact-awman-cli-{suite}.xml"));
        let records = crate::evidence::junit::parse(&std::fs::read_to_string(&junit)?)?;
        ensure!(
            records
                .iter()
                .map(|r| r.test.clone())
                .collect::<BTreeSet<_>>()
                == want,
            "{suite}: JUnit differs from compiled smoke inventory"
        );
        ensure!(
            records
                .iter()
                .all(|r| r.outcome == crate::evidence::schema::Outcome::Passed),
            "{suite}: smoke result is failed, blocked or skipped"
        );
        expected.extend(want.iter().map(|id| format!("{suite}::{id}")));
        results.extend(
            records
                .iter()
                .map(|r| json!({"test":format!("{suite}::{}",r.test),"outcome":"passed"})),
        );
        raw.push(json!({"suite":suite,"run_sha256":sha256_file(&run_path)?,
            "expected_inventory_sha256":sha256_file(&path)?,"junit_sha256":sha256_file(&junit)?}));
    }
    Ok(json!({"expected":expected,"results":results,"raw":raw}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_smoke_requires_the_compiled_inventory_and_passing_junit() {
        let tmp = tempfile::tempdir().unwrap();
        let plan: Plan = serde_json::from_slice(include_bytes!(
            "../../../tests/release_contract/fixtures/release-plan.json"
        ))
        .unwrap();
        let mut asset = plan.assets[0].clone();
        asset.smoke_suites = vec!["artifact-smoke".into()];
        let out = tmp.path().join("artifact-smoke");
        std::fs::create_dir(&out).unwrap();
        let run = json!({"schema":1,"runner":"nextest","build":asset.build,"failed_suites":[],
            "source":{"commit":"R","tree":"T","clean":true},
            "invocations":[{"exit":0,"suites":["artifact-smoke"],"expected":"expected-artifact-smoke.json"}]});
        write_json(&out.join("run.json"), &run).unwrap();
        write_json(&out.join("expected-artifact-smoke.json"),&json!({"rust-suites":{"artifact":{
            "binary-id":"awman-cli::artifact","testcases":{"smoke::starts":{"ignored":false,"filter-match":{"status":"matches"}}}}}})).unwrap();
        let junit = out.join("junit-artifact-awman-cli-artifact-smoke.xml");
        std::fs::write(&junit,"<testsuites><testsuite><testcase classname=\"awman-cli::artifact\" name=\"smoke::starts\"/></testsuite></testsuites>").unwrap();
        assert_eq!(
            read(tmp.path(), &asset, "R", "T").unwrap()["expected"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for xml in ["<testsuites/>","<testsuite><testcase classname=\"awman-cli::artifact\" name=\"smoke::starts\"><skipped/></testcase></testsuite>"] {
            std::fs::write(&junit,xml).unwrap();
            assert!(read(tmp.path(),&asset,"R","T").is_err());
        }
    }
}
