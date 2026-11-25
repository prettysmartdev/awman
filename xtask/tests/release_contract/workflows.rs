use super::*;
use serde_json::{json, Value};

/// GitHub rejects a reusable workflow before any job starts when one of its
/// jobs asks for a token scope withheld by the calling job. Check every local
/// workflow call, including calls behind an `if` that would skip at runtime.
#[test]
fn local_reusable_workflow_calls_grant_nested_job_permissions() {
    use serde_yaml::Value as Yaml;

    fn rank(value: Option<&Yaml>) -> u8 {
        match value.and_then(Yaml::as_str) {
            Some("read") => 1,
            Some("write") => 2,
            _ => 0,
        }
    }

    for entry in fs::read_dir(root().join(".github/workflows")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("yml") {
            continue;
        }
        let caller: Yaml = serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for (job_id, job) in caller["jobs"].as_mapping().unwrap() {
            let Some(called_path) = job["uses"].as_str().and_then(|s| s.strip_prefix("./")) else {
                continue;
            };
            let called: Yaml =
                serde_yaml::from_str(&fs::read_to_string(root().join(called_path)).unwrap())
                    .unwrap();
            let granted = job.get("permissions").unwrap_or(&caller["permissions"]);
            for (nested_id, nested_job) in called["jobs"].as_mapping().unwrap() {
                let required = nested_job
                    .get("permissions")
                    .unwrap_or(&called["permissions"]);
                for (scope, level) in required.as_mapping().unwrap() {
                    let given = granted.as_mapping().and_then(|scopes| scopes.get(scope));
                    let given_rank = if granted.as_str() == Some("read-all") {
                        1
                    } else {
                        rank(given)
                    };
                    assert!(
                        given_rank >= rank(Some(level)),
                        "{} job {} calls {} job {} requiring {}={}, but grants {}",
                        path.display(),
                        job_id.as_str().unwrap(),
                        called_path,
                        nested_id.as_str().unwrap(),
                        scope.as_str().unwrap(),
                        level.as_str().unwrap(),
                        given.and_then(Yaml::as_str).unwrap_or("none")
                    );
                }
            }
        }
    }
}

/// The complete expression is checked before evaluating its two format strings
/// locally. GitHub admission/ordering itself needs an Actions rehearsal.
fn group(repo: &str, version: &str, dry_run: bool, run: u64, record: &str) -> String {
    let workflow: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(root().join(".github/workflows/candidate.yml")).unwrap(),
    )
    .unwrap();
    let expression = workflow["concurrency"]["group"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(expression, "${{ toJSON(fromJSON(inputs.operator_record || '{}').record_only) == 'true' && format('candidate-journal-{0}-{1}', github.repository, github.run_id) || format('candidate-{0}-{1}-{2}', github.repository, inputs.version, inputs.dry_run) }}");
    assert_eq!(
        workflow["concurrency"]["cancel-in-progress"].as_bool(),
        Some(false)
    );
    let record: Value = serde_json::from_str(record).unwrap();
    if record["record_only"] == true {
        format!("candidate-journal-{repo}-{run}")
    } else {
        format!("candidate-{repo}-{version}-{dry_run}")
    }
}

#[test]
fn candidate_keys_serialize_same_repository_version_and_mode_across_runs() {
    let key = group("example/awman", "v1.2.3", false, 1, "{}");
    assert_eq!(key, group("example/awman", "v1.2.3", false, 2, "{}"));
    for (repo, version, dry) in [
        ("other/awman", "v1.2.3", false),
        ("example/awman", "v1.2.4", false),
        ("example/awman", "v1.2.3", true),
    ] {
        assert_ne!(key, group(repo, version, dry, 1, "{}"));
    }
}

#[test]
fn only_boolean_journals_get_independent_run_specific_keys() {
    let normal = group("example/awman", "v1.2.3", false, 1, "{}");
    for record in [
        r#"{"record_only":true}"#,
        "{\n\t\"record_only\"  :  true\n}",
    ] {
        let journal = group("example/awman", "v1.2.3", false, 1, record);
        assert_ne!(journal, normal);
        assert_ne!(journal, group("example/awman", "v1.2.3", false, 2, record));
    }
    for record in [
        json!({"record_only": false}),
        json!({"record_only": "true"}),
        json!({"record_only": 1}),
        json!({"record_only": "1"}),
        json!({"record_only": null}),
        json!({"note": "record_only:true"}),
    ] {
        assert_eq!(
            normal,
            group("example/awman", "v1.2.3", false, 2, &record.to_string())
        );
    }
}

#[test]
fn fixture_publication_attests_both_archives_and_the_reproduction_record() {
    let text = fs::read_to_string(root().join(".github/workflows/fixtures.yml")).unwrap();
    assert!(text
        .contains("fixtures verify-build --base base.tar --sha256 \"$BASE_SHA256\" --out built"));
    assert!(text.contains("BASE_IMAGE=$(cat ci/fixture-base-image.txt)"));
    assert!(!text.contains("base_url"));
    assert!(text.contains(
        "out/fixture-oci.tar\n            out/fixture-docker.tar\n            out/provenance.json\n            out/base-image.txt"
    ));
    assert!(text.contains("Refusing to replace an existing fixture release"));
    assert!(!text.contains("--clobber"));
    assert!(text.contains(
        "gh release create \"$TAG\" --target \"$COMMIT\" --title \"$TAG\" --prerelease --latest=false"
    ));
}

#[test]
fn release_publication_belongs_to_the_operator() {
    let text = fs::read_to_string(root().join(".github/workflows/promote.yml")).unwrap();
    assert!(!text.contains("create-github-app-token"));
    assert!(!text.contains("release promote tag"));
    assert!(!text.contains("release promote publish"));
    assert!(text.contains("release verify public"));
    let operator = fs::read_to_string(root().join("tools/prerelease/publish.sh")).unwrap();
    assert!(operator.contains("release promote tag"));
    assert!(operator.contains("release promote publish"));
}
