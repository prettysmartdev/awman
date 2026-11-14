use super::{schema::*, *};
use chrono::{TimeZone, Utc};
use std::collections::{BTreeMap, BTreeSet};
fn result(id: &str) -> ResultRecord {
    ResultRecord {
        test: id.into(),
        outcome: Outcome::Passed,
        duration_ms: 1,
        classification: None,
        reason: None,
        capabilities_lost: vec![],
        assertions: vec!["contract".into()],
    }
}
fn expected() -> ExpectedSet {
    ExpectedSet {
        schema: 1,
        build: "b".into(),
        cell: "c".into(),
        manifest_version: "m".into(),
        archive_sha256: "1".repeat(64),
        tests: BTreeSet::from(["p::t case::a".into()]),
        partitions: BTreeMap::from([(1, BTreeSet::from(["p::t case::a".into()]))]),
        contracts: BTreeMap::from([("p::t case::a".into(), BTreeSet::from(["contract".into()]))]),
    }
}
#[test]
fn exact_set_passes_and_equal_totals_substitution_fails() {
    let e = expected();
    aggregate::compare(&e, &[result("p::t case::a")]).unwrap();
    assert!(aggregate::compare(&e, &[result("p::t case::b")])
        .unwrap_err()
        .to_string()
        .contains("substituted"));
}
#[test]
fn missing_skipped_inconclusive_and_blocked_required_results_fail() {
    let e = expected();
    assert!(aggregate::compare(&e, &[])
        .unwrap_err()
        .to_string()
        .contains("did not run"));
    for outcome in [
        Outcome::Skipped,
        Outcome::Inconclusive,
        Outcome::Blocked,
        Outcome::Failed,
        Outcome::NotSelected,
    ] {
        let mut r = result("p::t case::a");
        r.outcome = outcome;
        assert!(aggregate::compare(&e, &[r]).is_err());
    }
}
#[test]
fn duplicate_empty_and_early_return_evidence_fail() {
    let mut e = expected();
    let mut r = result("p::t case::a");
    assert!(aggregate::compare(&e, &[r.clone(), r.clone()])
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
    r.assertions.clear();
    assert!(aggregate::compare(&e, &[r])
        .unwrap_err()
        .to_string()
        .contains("early-return/inconclusive"));
    e.tests.clear();
    assert!(aggregate::compare(&e, &[])
        .unwrap_err()
        .to_string()
        .contains("empty required"));
}
#[test]
fn capability_loss_cannot_be_a_pass() {
    let mut r = result("p::t case::a");
    r.capabilities_lost.push("docker".into());
    assert!(aggregate::compare(&expected(), &[r]).is_err());
}
#[test]
fn junit_classifies_timeout_blocked_and_skipped() {
    let rs=junit::parse("<testsuites><testsuite><testcase classname='p::t' name='a'/><testcase classname='p::t' name='b'><failure type='test timeout'>timeout</failure></testcase><testcase classname='p::t' name='c'><failure message='BLOCKED[docker]: unavailable'/></testcase><testcase classname='p::t' name='d'><skipped/></testcase></testsuite></testsuites>").unwrap();
    assert_eq!(
        rs.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        vec![
            Outcome::Passed,
            Outcome::Failed,
            Outcome::Blocked,
            Outcome::Skipped
        ]
    );
    let c = junit::counts(4, &rs);
    assert_eq!(c.ran, 3);
    assert_eq!(c.skipped, 1);
    assert!(junit::parse("<testsuite><testcase classname='p' name='a'/><testcase classname='p' name='a'/></testsuite>").is_err());
}
#[test]
fn digest_is_canonical_and_nulls_and_notes_matter() {
    let a = serde_json::json!({"z":null,"a":1});
    let b = serde_json::json!({"a":1,"z":null});
    assert_eq!(
        digest::input_digest(&a).unwrap(),
        digest::input_digest(&b).unwrap()
    );
    assert_ne!(
        digest::input_digest(&a).unwrap(),
        digest::input_digest(&serde_json::json!({"a":1})).unwrap()
    );
    assert_ne!(
        aggregate::candidate_digest(
            &BTreeMap::from([("notes".into(), "old".into())]),
            &BTreeMap::new()
        )
        .unwrap(),
        aggregate::candidate_digest(
            &BTreeMap::from([("notes".into(), "new".into())]),
            &BTreeMap::new()
        )
        .unwrap()
    );
}
fn context() -> (
    crate::manifest::Manifest,
    crate::gates::plan::Plan,
    TrustedContext,
    Vec<Evidence>,
) {
    let mut m = crate::gates::tests::manifest();
    // Synthetic controller fixture. Real missing pins remain red.
    for t in m.tools.values_mut() {
        if t.sha256.is_empty() {
            t.sha256.insert("test-controller".into(), "f".repeat(64));
        }
    }
    for f in m.fixtures.values_mut() {
        if f.verified_record.is_none() {
            f.sha256 = Some("e".repeat(64));
        }
    }
    for p in m.previous_release.values_mut() {
        p.sha256 = Some("d".repeat(64));
    }
    for p in m.platforms.values_mut() {
        if p.image.starts_with("pending") {
            p.image = "test-controller-image".into();
        }
    }
    m.version = digest::input_digest(&m).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
    let request = crate::gates::tests::request("pull_request", &["src/data/config.rs"]);
    let p = crate::gates::plan::create(&m, "pr", request.clone(), now.date_naive()).unwrap();
    let pd = p.digest().unwrap();
    let mut ctx = TrustedContext {
        request,
        plan_sha256: pd.clone(),
        producers: vec![],
        expected: BTreeMap::new(),
        input_digests: BTreeMap::new(),
        archive_digests: BTreeMap::new(),
        jobs: ["plan", "lint", "build", "test"]
            .iter()
            .map(|s| (s.to_string(), Outcome::Passed))
            .collect(),
        now,
    };
    let mut records = Vec::new();
    for (i, c) in p.cells.iter().filter(|c| c.selected).enumerate() {
        let build = &m.builds[&c.cell.build];
        let platform = &m.platforms[&c.cell.platform];
        let suite = m.suites.get(&c.cell.suite);
        let inputs = Inputs {
            schema: 1,
            source_tree: "b".repeat(40),
            cargo_manifests: [
                "Cargo.toml",
                "xtask/Cargo.toml",
                "crates/awman-cli/Cargo.toml",
                "crates/awman-host-shims/Cargo.toml",
                "crates/awman-wsp/Cargo.toml",
                "crates/awman-test-support/Cargo.toml",
            ]
            .iter()
            .map(|s| (s.to_string(), "c".repeat(64)))
            .collect(),
            cargo_lock: "c".repeat(64),
            toolchain: platform.toolchain.clone(),
            tool_digests: BTreeMap::from([("nextest".into(), "f".repeat(64))]),
            rust_toolchain_file: None,
            cargo_config: "d".repeat(64),
            build_scripts: BTreeMap::from([("build.rs".into(), "f".repeat(64))]),
            payload_manifest: Some("e".repeat(64)),
            features: m.features[&build.features].cargo_args.clone(),
            target: build
                .target
                .clone()
                .unwrap_or_else(|| platform.triple.clone()),
            profile: build.profile.clone(),
            runner_image: platform.image.clone(),
            fixtures: BTreeMap::new(),
            previous_release: None,
            signing: None,
            release_notes: None,
        };
        let id = format!("{} contract_case", c.cell.suite);
        let mut expected = expected();
        expected.build = c.cell.build.clone();
        expected.cell = c.cell.id.clone();
        expected.manifest_version = m.version().into();
        expected.tests = BTreeSet::from([id.clone()]);
        expected.partitions = BTreeMap::from([(1, expected.tests.clone())]);
        expected.contracts = BTreeMap::from([(id.clone(), BTreeSet::from(["contract".into()]))]);
        let input_digest = digest::input_digest(&inputs).unwrap();
        ctx.input_digests
            .insert(c.cell.build.clone(), input_digest.clone());
        ctx.archive_digests
            .insert(c.cell.build.clone(), expected.archive_sha256.clone());
        let r = Evidence {
            schema: 1,
            kind: "cell".into(),
            gate: "pr".into(),
            cell: c.cell.id.clone(),
            suite: c.cell.suite.clone(),
            tier: suite.map_or("unit", |s| s.tier.as_str()).into(),
            backend: suite.and_then(|s| s.backend.clone()),
            build: c.cell.build.clone(),
            platform: c.cell.platform.clone(),
            shard: Shard { index: 1, of: 1 },
            commit: ctx.request.head_sha.clone(),
            tree: inputs.source_tree.clone(),
            manifest_version: m.version().into(),
            input_digest,
            inputs,
            expected_set_sha256: digest::input_digest(&expected).unwrap(),
            archive_sha256: expected.archive_sha256.clone(),
            plan_sha256: pd.clone(),
            artifacts: vec![],
            counts: junit::counts(1, &[result(&id)]),
            results: vec![result(&id)],
            release_critical: vec![],
            outcome: Outcome::Passed,
            classification: None,
            durations: Durations::default(),
            cache: None,
            reproduce: "cargo xtask test run".into(),
            run: Run {
                provider: "github-actions".into(),
                repository_id: "42".into(),
                workflow_path: ".github/workflows/ci.yml".into(),
                workflow_revision: "a".repeat(40),
                run_id: 1,
                attempt: 1,
                job: "test".into(),
                job_id: i as u64 + 1,
                artifact_id: i as u64 + 1,
                artifact_service_digest: "4".repeat(64),
                url: "https://example.test/run".into(),
            },
            started_at: now - chrono::Duration::seconds(2),
            finished_at: now - chrono::Duration::seconds(1),
            expires_at: now + chrono::Duration::days(1),
            command_exit: 0,
            phases: ["setup", "run", "sweep", "upload"]
                .iter()
                .map(|p| (p.to_string(), Outcome::Passed))
                .collect(),
            capabilities_probed: suite.map_or_else(
                || BTreeSet::from(["tempdir".into()]),
                |s| {
                    m.tiers[&s.tier]
                        .guarantees
                        .iter()
                        .chain(&s.requires)
                        .cloned()
                        .collect()
                },
            ),
        };
        ctx.expected.insert(c.cell.id.clone(), expected);
        ctx.producers.push(VerifiedProducer {
            identity: r.run.clone(),
            evidence_sha256: digest::input_digest(&r).unwrap(),
            conclusion: Outcome::Passed,
            subject_sha256: r.archive_sha256.clone(),
        });
        records.push(r);
    }
    (m, p, ctx, records)
}
fn resign(ctx: &mut TrustedContext, r: &Evidence) {
    let producer = ctx
        .producers
        .iter_mut()
        .find(|p| p.identity.job_id == r.run.job_id)
        .unwrap();
    producer.identity = r.run.clone();
    producer.evidence_sha256 = digest::input_digest(r).unwrap();
}
#[test]
fn aggregate_valid_authenticated_exact_sets_pass() {
    let (m, p, c, r) = context();
    let g = aggregate::aggregate(&m, &p, &r, &c).unwrap();
    assert_eq!(g.verdict, Outcome::Passed, "{:?}", g.errors);
    assert_eq!(g.execution_ms, 1000);
}
#[test]
fn missing_suite_fails_closed() {
    let (m, p, c, mut r) = context();
    r.pop();
    let g = aggregate::aggregate(&m, &p, &r, &c).unwrap();
    assert_eq!(g.verdict, Outcome::Blocked);
    assert!(!g.missing.is_empty());
}
#[test]
fn reduced_or_stale_plan_is_rejected() {
    let (m, mut p, c, r) = context();
    p.cells.pop();
    assert!(aggregate::aggregate(&m, &p, &r, &c)
        .unwrap_err()
        .to_string()
        .contains("independently reconstructed"));
}
#[test]
fn local_forged_and_renamed_job_cannot_satisfy_gate() {
    let (m, p, c, mut r) = context();
    r[0].run.provider = "local".into();
    let g = aggregate::aggregate(&m, &p, &r, &c).unwrap();
    assert_eq!(g.verdict, Outcome::Failed);
    assert!(g.errors.iter().any(|e| e.contains("authenticated")));
    let (m, p, c, mut r) = context();
    r[0].run.job = "renamed".into();
    assert_eq!(
        aggregate::aggregate(&m, &p, &r, &c).unwrap().verdict,
        Outcome::Failed
    );
}
#[test]
fn skip_expiry_failed_phase_and_capability_loss_are_red() {
    for fault in [
        "skip",
        "expiry",
        "sweep",
        "capability",
        "assertion",
        "exit",
        "attempt",
        "archive",
        "input",
    ] {
        let (m, p, mut c, mut r) = context();
        match fault {
            "skip" => r[0].results[0].outcome = Outcome::Skipped,
            "expiry" => r[0].expires_at = c.now,
            "sweep" => {
                r[0].phases.insert("sweep".into(), Outcome::Failed);
            }
            "capability" => r[0].capabilities_probed.clear(),
            "assertion" => r[0].results[0].assertions.clear(),
            "exit" => r[0].command_exit = 1,
            "attempt" => r[0].run.attempt = 0,
            "archive" => r[0].archive_sha256 = "5".repeat(64),
            "input" => r[0].inputs.release_notes = Some("6".repeat(64)),
            _ => unreachable!(),
        };
        resign(&mut c, &r[0]);
        let g = aggregate::aggregate(&m, &p, &r, &c).unwrap();
        assert_eq!(g.verdict, Outcome::Failed, "{fault}");
        assert!(!g.errors.is_empty());
    }
}
#[test]
fn duplicate_shard_and_substituted_id_fail_even_when_totals_match() {
    let (m, p, c, mut r) = context();
    r.push(r[0].clone());
    assert_eq!(
        aggregate::aggregate(&m, &p, &r, &c).unwrap().verdict,
        Outcome::Failed
    );
    let (m, p, mut c, mut r) = context();
    r[0].results[0].test = "substitution".into();
    resign(&mut c, &r[0]);
    let g = aggregate::aggregate(&m, &p, &r, &c).unwrap();
    assert!(g.errors.iter().any(|s| s.contains("substituted")));
    assert!(g.errors.iter().any(|s| s.contains("did not run")));
}
#[test]
fn writer_round_trip_and_emit_never_infers_assertions() {
    let (m, p, c, r) = context();
    let e = &r[0];
    let expected = &c.expected[&e.cell];
    let parsed = junit::parse(&format!(
        "<testsuite><testcase classname='{}' name='contract_case'/></testsuite>",
        e.suite
    ))
    .unwrap();
    let red = emit::finish(e.clone(), expected, parsed).unwrap();
    assert_ne!(red.outcome, Outcome::Passed);
    let dir = tempfile::tempdir().unwrap();
    emit::write(dir.path(), e).unwrap();
    let read = emit::read_directory(dir.path()).unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(
        digest::input_digest(e).unwrap(),
        digest::input_digest(&read[0]).unwrap()
    );
    assert!(
        summary::render(&aggregate::aggregate(&m, &p, &r, &c).unwrap(), &r)
            .contains("Slowest tests")
    );
}
#[test]
fn history_percentiles_and_three_run_budget() {
    assert_eq!(history::percentile(&[1, 2, 3, 4, 5], 95), Some(5));
    assert_eq!(history::percentile(&[], 50), None);
    let sample = history::Sample {
        schema: 1,
        gate: "main".into(),
        cell: "c".into(),
        tier: "gate".into(),
        run_id: 1,
        attempt: 1,
        shard: None,
        manifest_version: "v".into(),
        commit: "a".repeat(40),
        tests: BTreeMap::new(),
        queue_ms: Some(0),
        execution_ms: 100,
        outcome: Outcome::Passed,
        cache_hit: None,
        rebuilds: Some(1),
        manual_interventions: Some(0),
    };
    let mut rows = vec![sample.clone()];
    assert!(!history::budget_regression(&rows, 50));
    for id in [2, 3] {
        let mut s = sample.clone();
        s.run_id = id;
        rows.push(s);
    }
    assert!(history::budget_regression(&rows, 50));
    assert!(history::report(&rows).contains("100"));
}
/// Guards: 824aeda9, d0a29ec2, 4a2853d7.
#[test]
fn missing_plan_job_and_incomplete_inputs_cannot_be_green() {
    let (m, p, mut c, r) = context();
    c.jobs.remove("plan");
    assert!(aggregate::aggregate(&m, &p, &r, &c)
        .unwrap()
        .errors
        .iter()
        .any(|s| s.contains("required job plan")));
    let (m, p, mut c, mut r) = context();
    r[0].inputs.cargo_manifests.clear();
    r[0].input_digest = digest::input_digest(&r[0].inputs).unwrap();
    c.input_digests
        .insert(r[0].build.clone(), r[0].input_digest.clone());
    resign(&mut c, &r[0]);
    assert!(aggregate::aggregate(&m, &p, &r, &c)
        .unwrap()
        .errors
        .iter()
        .any(|s| s.contains("missing Cargo manifest digest")));
}
#[test]
fn published_json_schema_tracks_serialized_field_sets() {
    let (_, _, _, records) = context();
    let record = &records[0];
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../../ci/schemas/evidence-v1.json")).unwrap();
    fn keys(value: serde_json::Value) -> BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }
    assert_eq!(
        keys(serde_json::to_value(record).unwrap()),
        keys(schema["properties"].clone())
    );
    assert_eq!(
        keys(serde_json::to_value(&record.inputs).unwrap()),
        keys(schema["$defs"]["Inputs"]["properties"].clone())
    );
    assert_eq!(
        keys(serde_json::to_value(&record.durations).unwrap()),
        keys(schema["$defs"]["Durations"]["properties"].clone())
    );
}
#[test]
fn streaming_file_digest_matches_known_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("artifact");
    std::fs::write(&path, b"abc").unwrap();
    assert_eq!(
        digest::sha256_file(&path).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let bytes = vec![42; 200_000];
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(digest::sha256_file(&path).unwrap(), digest::sha256(&bytes));
}
#[test]
fn contradictory_runtime_records_are_rejected() {
    let mut results = vec![result("case")];
    let mut record = result("case");
    record.outcome = Outcome::Skipped;
    assert!(emit::merge_records(&mut results, &[record])
        .unwrap_err()
        .to_string()
        .contains("contradictory"));
}
#[test]
fn shard_partitions_must_be_exact_disjoint_and_complete() {
    let mut e = expected();
    e.tests.insert("p::t case::b".into());
    e.partitions
        .insert(2, BTreeSet::from(["p::t case::b".into()]));
    aggregate::partitions(&e, 2).unwrap();
    e.partitions
        .get_mut(&2)
        .unwrap()
        .insert("p::t case::a".into());
    assert!(aggregate::partitions(&e, 2)
        .unwrap_err()
        .to_string()
        .contains("duplicate expected test"));
    e.partitions.remove(&2);
    assert!(aggregate::partitions(&e, 2)
        .unwrap_err()
        .to_string()
        .contains("missing/extra"));
}
#[test]
fn emitter_compares_only_its_independently_listed_partition() {
    let (_, _, ctx, records) = context();
    let mut e = records[0].clone();
    let mut expected = ctx.expected[&e.cell].clone();
    let other = "other shard test".to_string();
    expected.tests.insert(other.clone());
    expected
        .partitions
        .insert(2, BTreeSet::from([other.clone()]));
    expected
        .contracts
        .insert(other, BTreeSet::from(["contract".into()]));
    e.shard.of = 2;
    let results = e.results.clone();
    let out = emit::finish(e, &expected, results).unwrap();
    assert_eq!(out.outcome, Outcome::Passed);
    assert_eq!(out.counts.expected, 1);
}

#[test]
fn slow_history_cannot_block_the_fixing_pr_or_an_improved_main_run() {
    let history = [101, 102, 103];
    assert!(aggregate::budget_findings("pr", &history, 200, 100).is_empty());
    assert!(aggregate::budget_findings("main", &history, 100, 100).is_empty());
    assert!(!aggregate::budget_findings("main", &history, 101, 100).is_empty());
}
