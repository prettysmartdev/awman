use super::*;
use serde_json::{json, Value};

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
}

#[test]
fn rehearsal_mints_the_promotion_identity_without_publishing() {
    let text = fs::read_to_string(root().join(".github/workflows/promote.yml")).unwrap();
    assert!(text.contains("Verify the promotion App can mint a write token without publishing"));
    assert!(text.contains("id: rehearsal_app\n        if: ${{ inputs.dry_run }}\n        uses: actions/create-github-app-token@"));
    assert!(text.contains("Verify that App is the v-tag ruleset bypass"));
    assert!(text.contains("if: ${{ !inputs.dry_run }}"));
    assert!(text.contains("permission-contents: write"));
}
