//! Unit tests for every release evidence and promotion decision (WI 5.5,
//! design 12.4 and 16.1/16.4).
use super::tests_support::*;
use super::*;
use crate::manifest::Override;
use chrono::Duration;

fn id(rec: &ReleaseRecord) -> Identity {
    rec.identity()
}

fn stop_text(r: Result<impl std::fmt::Debug, Stop>) -> String {
    match r {
        Err(s) => s.to_string(),
        Ok(v) => panic!("expected a stop, got {v:?}"),
    }
}

// ── resolve ──────────────────────────────────────────────────────────────────

#[test]
fn first_candidate_is_number_one_and_abandoned_numbers_are_never_reused() {
    let rec = record();
    assert_eq!(
        resolve(&id(&rec), &[], 0, None),
        Ok(Resolution::New { candidate: 1 })
    );
    assert_eq!(
        resolve(&id(&rec), &[], 3, None),
        Ok(Resolution::New { candidate: 4 })
    );
}

#[test]
fn same_identity_with_complete_draft_resumes_without_building() {
    let rec = record();
    assert_eq!(
        resolve(&id(&rec), &[existing(&rec)], 1, None),
        Ok(Resolution::Resume {
            candidate: 1,
            release_id: 500,
            state: ReleaseState::Validated
        })
    );
}

#[test]
fn changed_input_invalidates_evidence_and_allocates_a_new_candidate() {
    let rec = record();
    type Change = Box<dyn Fn(&mut Identity)>;
    let cases: Vec<(&str, Change)> = vec![
        (
            "lockfile/toolchain/payload/fixture/entitlement/notes",
            Box::new(|i| i.input_digest = "9".repeat(64)),
        ),
        ("manifest", Box::new(|i| i.manifest_version = "m2".into())),
        (
            "force-pushed commit",
            Box::new(|i| i.commit = "2".repeat(40)),
        ),
    ];
    for (what, change) in cases {
        let mut new = id(&rec);
        change(&mut new);
        assert_eq!(
            resolve(&new, &[existing(&rec)], 1, None),
            Ok(Resolution::New { candidate: 2 }),
            "{what} must not reuse candidate 1"
        );
    }
    // Every recorded input field feeds the digest the identity uses.
    let mut changed = rec.inputs.clone();
    changed.entitlements_sha256 = Some("0".repeat(64));
    assert_ne!(changed.digest().unwrap(), rec.input_digest);
    assert_eq!(
        changed.diff(&rec.inputs),
        vec!["entitlements_sha256".to_string()]
    );
    let mut notes = rec.inputs.clone();
    notes.release_notes_sha256 = Some("0".repeat(64));
    assert_ne!(notes.digest().unwrap(), rec.input_digest);
    let mut lock = rec.inputs.clone();
    lock.cargo_lock = "0".repeat(64);
    assert_ne!(
        lock.target_digest("awman-linux-amd64").unwrap(),
        rec.artifacts[0].input_digest
    );
}

#[test]
fn partial_draft_of_another_run_is_superseded_not_resumed() {
    let mut rec = record();
    rec.state = ReleaseState::Prepared;
    assert_eq!(
        resolve(&id(&rec), &[existing(&rec)], 1, None),
        Ok(Resolution::New { candidate: 2 })
    );
    let mut lost = existing(&record());
    lost.release
        .assets
        .retain(|a| a.name != "awman-macos-arm64");
    assert_eq!(
        resolve(&id(&record()), &[lost], 1, None),
        Ok(Resolution::New { candidate: 2 }),
        "lost bytes are never rebuilt under the old identity"
    );
}

#[test]
fn published_versions_are_never_rebuilt_moved_or_reused() {
    let rec = record();
    let mut published = existing(&rec);
    published.release.draft = false;
    assert_eq!(
        resolve(&id(&rec), &[published.clone()], 1, Some(COMMIT)),
        Ok(Resolution::Published {
            release_id: 500,
            candidate: Some(1)
        })
    );
    let mut other = id(&rec);
    other.commit = "2".repeat(40);
    assert!(stop_text(resolve(&other, &[published], 1, Some(COMMIT))).contains("never moved"));
    assert!(stop_text(resolve(&id(&rec), &[], 0, Some(&"3".repeat(40)))).starts_with("FAILED"));
}

// ── assembly / concurrency ───────────────────────────────────────────────────

#[test]
fn concurrent_candidates_for_one_version_block_without_publishing() {
    let rec = record();
    let mut theirs = existing(&rec);
    theirs.release.body = run_marker(99);
    let r = assembly(&id(&rec), 1, 7, &[theirs.clone()]);
    assert!(
        stop_text(r).starts_with("BLOCKED: candidate 1"),
        "same number from another run"
    );
    let mut newer = theirs.clone();
    newer.release.name = super::super::record::draft_name("v0.14.0", 2);
    newer.record = None;
    assert!(stop_text(assembly(&id(&rec), 1, 7, &[newer])).starts_with("BLOCKED"));
    assert_eq!(
        assembly(&id(&rec), 2, 7, &[theirs]),
        Ok(Assembly::Create {
            supersede: vec![500]
        }),
        "an older candidate is superseded under the lock"
    );
    let mine = existing(&rec);
    assert_eq!(
        assembly(&id(&rec), 1, 7, &[mine]),
        Ok(Assembly::Continue {
            release_id: 500,
            supersede: vec![]
        }),
        "a rerun of assembly finishes its own draft"
    );
    let mut published = existing(&rec);
    published.release.draft = false;
    assert!(stop_text(assembly(&id(&rec), 2, 7, &[published])).starts_with("FAILED"));
}

#[test]
fn partial_asset_upload_resumes_only_the_missing_assets() {
    let rec = record();
    let mut rel = release(&rec);
    rel.assets
        .retain(|a| a.name != "awman-windows-amd64.exe" && a.name != "awman-linux-arm64");
    assert_eq!(
        missing_uploads(&rel, &rec),
        Ok(vec![
            "awman-linux-arm64".to_string(),
            "awman-windows-amd64.exe".to_string()
        ])
    );
    let mut changed = release(&rec);
    changed.assets[0].digest = Some(format!("sha256:{}", "0".repeat(64)));
    assert!(stop_text(missing_uploads(&changed, &rec)).contains("lost bytes"));
}

// ── evidence validity ────────────────────────────────────────────────────────

#[test]
fn valid_evidence_has_no_findings() {
    let rec = record();
    assert_eq!(
        evidence_findings(&rec, &manifest(), &current(&rec), true),
        vec![]
    );
}

#[test]
fn expired_or_void_evidence_cannot_be_used() {
    let rec = record();
    let m = manifest();
    let mut cur = current(&rec);
    cur.now = rec.expires_at;
    assert!(evidence_findings(&rec, &m, &cur, true)[0]
        .to_string()
        .contains("expired"));
    let mut cur = current(&rec);
    cur.main_contains_commit = false;
    assert!(evidence_findings(&rec, &m, &cur, true)[0]
        .to_string()
        .contains("no longer"));
    let mut gate_old = rec.clone();
    gate_old.gates[0].expires_at = now() - Duration::seconds(1);
    assert!(evidence_findings(&gate_old, &m, &current(&rec), true)
        .iter()
        .any(|s| s.to_string().contains("gate main evidence expired")));
}

#[test]
fn changed_notes_entitlement_or_manifest_on_main_invalidate_the_candidate() {
    let rec = record();
    let m = manifest();
    for (what, f) in [
        (
            "notes",
            Box::new(|p: &mut Policy| p.release_notes_sha256 = Some("0".repeat(64)))
                as Box<dyn Fn(&mut Policy)>,
        ),
        (
            "entitlements",
            Box::new(|p: &mut Policy| p.entitlements_sha256 = None),
        ),
        (
            "gates.toml",
            Box::new(|p: &mut Policy| p.manifest_version = "other".into()),
        ),
    ] {
        let mut cur = current(&rec);
        f(&mut cur.policy);
        let found = evidence_findings(&rec, &m, &cur, true);
        assert_eq!(found.len(), 1, "{what}: {found:?}");
        assert!(found[0].to_string().contains("new candidate"), "{what}");
    }
}

#[test]
fn record_must_be_internally_consistent() {
    let mut rec = record();
    rec.artifacts[1].input_digest = "0".repeat(64);
    assert!(evidence_findings(&rec, &manifest(), &current(&rec), true)
        .iter()
        .any(|s| s.to_string().contains("built from inputs other")));
}

#[test]
fn rehearsal_evidence_never_authorizes_real_promotion() {
    let mut rec = record();
    rec.dry_run = true;
    rec.namespace = "rehearsal/9".into();
    assert!(evidence_findings(&rec, &manifest(), &current(&rec), true)
        .iter()
        .any(|s| s.to_string().contains("rehearsal")));
    assert!(evidence_findings(&rec, &manifest(), &current(&rec), false).is_empty());
}

fn hosted_cell(m: &Manifest) -> String {
    manifest::validate::cells(m, "main")
        .unwrap()
        .into_iter()
        .find(|c| c.suite == "component-data")
        .expect("component-data in main")
        .id
}

fn with_override(m: &mut Manifest, gate: &str, cell: &str, expires: &str) {
    m.overrides.push(Override {
        gate: gate.into(),
        cell: cell.into(),
        reason: "flaky upstream".into(),
        expires: expires.into(),
        approved_by: "@maintainer".into(),
    });
}

#[test]
fn override_expiring_during_validation_invalidates_affected_evidence() {
    let mut m = manifest();
    let cell = hosted_cell(&m);
    with_override(&mut m, "main", &cell, "2026-11-12");
    let mut rec = record();
    rec.gates[0].overrides_applied = vec![cell.clone()];
    assert!(evidence_findings(&rec, &m, &current(&rec), true).is_empty());
    let mut later = current(&rec);
    later.now = "2026-11-12T00:00:01Z".parse().unwrap();
    let found = evidence_findings(&rec, &m, &later, true);
    assert!(
        found
            .iter()
            .any(|s| s.to_string().contains(&format!("override for {cell}"))),
        "{found:?}"
    );
    // Removing the entry from the manifest has the same effect.
    let found = evidence_findings(&rec, &manifest(), &current(&rec), true);
    assert!(found
        .iter()
        .any(|s| s.to_string().contains("no committed override")));
}

#[test]
fn expired_quarantine_invalidates_evidence() {
    let mut m = manifest();
    m.quarantine.push(crate::manifest::Quarantine {
        test: "awman-cli::component engine::x".into(),
        owner: "@o".into(),
        issue: "https://example.invalid/1".into(),
        expires: "2026-11-11".into(),
    });
    let rec = record();
    assert!(evidence_findings(&rec, &m, &current(&rec), true)
        .iter()
        .any(|s| s.to_string().contains("expired quarantine")));
}

// ── overrides ────────────────────────────────────────────────────────────────

#[test]
fn overrides_never_cover_hardware_guest_signed_boot_or_artifact_integrity() {
    let mut m = manifest();
    let today = now().date_naive();
    let hosted = hosted_cell(&m);
    with_override(&mut m, "main", &hosted, "2026-12-01");
    assert_eq!(override_allowed(&m, "main", &hosted, today), Ok(()));
    let cand = candidate_cells(&m).unwrap();
    for c in &cand {
        with_override(&mut m, "candidate", &c.id, "2026-12-01");
    }
    for c in &cand {
        let e = override_allowed(&m, "candidate", &c.id, today).unwrap_err();
        assert!(
            e.contains("hardware") || e.contains("integrity") || e.contains("release-critical"),
            "{}: {e}",
            c.id
        );
    }
    assert!(
        override_allowed(&m, "main", "guest-boot@macos-arm64-ci-guest-builtin", today).is_err()
    );
    let mut expired = manifest();
    with_override(&mut expired, "main", &hosted, "2026-11-11");
    assert!(override_allowed(&expired, "main", &hosted, today)
        .unwrap_err()
        .contains("expired"));
    assert!(
        override_allowed(&manifest(), "main", &hosted, today).is_err(),
        "only committed entries"
    );
}

// ── candidate gate ───────────────────────────────────────────────────────────

#[test]
fn candidate_gate_passes_only_with_main_and_every_bound_cell() {
    let m = manifest();
    let rec = record();
    let cells = all_cells(&m, &rec);
    let g = candidate_gate(&m, &rec, Some(&main_gate(&rec)), &cells, now()).unwrap();
    assert_eq!(g.verdict, Outcome::Passed, "{:?}", g.errors);
    assert!(g
        .release_critical
        .iter()
        .all(|s| s.outcome == Outcome::Passed));
    assert!(g
        .release_critical
        .iter()
        .any(|s| s.id == "artifact.signed-offline-session"));
    assert_eq!(g.cells.len(), cells.len() + 1);
    assert!(g.expires_at.unwrap() <= rec.expires_at);
}

#[test]
fn missing_main_gate_blocks_and_unauthenticated_main_fails() {
    let m = manifest();
    let rec = record();
    let cells = all_cells(&m, &rec);
    assert_eq!(
        candidate_gate(&m, &rec, None, &cells, now())
            .unwrap()
            .verdict,
        Outcome::Blocked
    );
    let mut forged = main_gate(&rec);
    forged.authenticated = false;
    assert_eq!(
        candidate_gate(&m, &rec, Some(&forged), &cells, now())
            .unwrap()
            .verdict,
        Outcome::Failed
    );
    let mut stale = main_gate(&rec);
    stale.evidence.commit = "2".repeat(40);
    assert_eq!(
        candidate_gate(&m, &rec, Some(&stale), &cells, now())
            .unwrap()
            .verdict,
        Outcome::Failed
    );
}

#[test]
fn runner_lost_mid_validation_is_blocked_not_a_pass_or_a_skip() {
    let m = manifest();
    let rec = record();
    let mut cells = all_cells(&m, &rec);
    let i = cells
        .iter()
        .position(|c| c.evidence.suite == "guest-boot")
        .unwrap();
    cells[i].evidence.results[0].capabilities_lost = vec!["hypervisor".into()];
    cells[i].evidence.results[0].outcome = Outcome::Blocked;
    cells[i].evidence.counts =
        crate::evidence::junit::counts(cells[i].evidence.results.len(), &cells[i].evidence.results);
    let g = candidate_gate(&m, &rec, Some(&main_gate(&rec)), &cells, now()).unwrap();
    assert_eq!(g.verdict, Outcome::Blocked, "{:?}", g.errors);
    let failing = failing_gates(&g);
    assert!(failing
        .iter()
        .any(|f| f.outcome == "blocked" && f.reason.contains("lost hypervisor")));
    // The cell disappearing entirely is also blocked.
    let mut gone = all_cells(&m, &rec);
    gone.retain(|c| c.evidence.suite != "guest-builder");
    assert_eq!(
        candidate_gate(&m, &rec, Some(&main_gate(&rec)), &gone, now())
            .unwrap()
            .verdict,
        Outcome::Blocked
    );
}

#[test]
fn failed_suite_rerun_on_the_same_artifacts_turns_the_gate_green() {
    let m = manifest();
    let rec = record();
    let mut cells = all_cells(&m, &rec);
    let i = cells
        .iter()
        .position(|c| c.evidence.suite == "artifact-smoke")
        .unwrap();
    cells[i].evidence.results[0].outcome = Outcome::Failed;
    cells[i].evidence.outcome = Outcome::Failed;
    let g = candidate_gate(&m, &rec, Some(&main_gate(&rec)), &cells, now()).unwrap();
    assert_eq!(g.verdict, Outcome::Failed);
    // `gh run rerun --failed` replaces only that cell's evidence; bytes are unchanged.
    cells[i] = cell(
        &m,
        &rec,
        &candidate_cells(&m)
            .unwrap()
            .into_iter()
            .find(|c| c.id == cells[i].evidence.cell)
            .unwrap(),
    );
    cells[i].evidence.run.attempt = 2;
    let g = candidate_gate(&m, &rec, Some(&main_gate(&rec)), &cells, now()).unwrap();
    assert_eq!(g.verdict, Outcome::Passed, "{:?}", g.errors);
}

#[test]
fn skipped_unbound_stale_or_forged_cells_fail() {
    let m = manifest();
    let rec = record();
    type Mutate = Box<dyn Fn(&mut CellRecord)>;
    let cases: Vec<(&str, Mutate)> = vec![
        (
            "skip",
            Box::new(|c| c.evidence.results[0].outcome = Outcome::Skipped),
        ),
        (
            "other bytes",
            Box::new(|c| {
                if let Some(a) = c.evidence.artifacts.first_mut() {
                    a.sha256 = "0".repeat(64)
                } else {
                    c.evidence.commit = "0".repeat(40)
                }
            }),
        ),
        (
            "stale commit",
            Box::new(|c| c.evidence.commit = "2".repeat(40)),
        ),
        (
            "old manifest",
            Box::new(|c| c.evidence.manifest_version = "old".into()),
        ),
        ("forged/local", Box::new(|c| c.authenticated = false)),
        ("no tests", Box::new(|c| c.evidence.results.clear())),
        (
            "failed sweep phase",
            Box::new(|c| {
                c.evidence.phases.insert("sweep".into(), Outcome::Failed);
            }),
        ),
        ("expired", Box::new(|c| c.evidence.expires_at = now())),
    ];
    for (what, mutate) in cases {
        let mut cells = all_cells(&m, &rec);
        let i = cells
            .iter()
            .position(|c| c.evidence.suite == "artifact-signed-boot")
            .unwrap();
        mutate(&mut cells[i]);
        let g = candidate_gate(&m, &rec, Some(&main_gate(&rec)), &cells, now()).unwrap();
        assert_eq!(g.verdict, Outcome::Failed, "{what}: {:?}", g.errors);
    }
    let mut unknown = all_cells(&m, &rec);
    let mut extra = unknown[0].clone();
    extra.evidence.cell = "artifact-smoke@nowhere".into();
    unknown.push(extra);
    assert_eq!(
        candidate_gate(&m, &rec, Some(&main_gate(&rec)), &unknown, now())
            .unwrap()
            .verdict,
        Outcome::Failed
    );
}

#[test]
fn override_attempted_on_a_not_overridable_cell_is_refused() {
    let mut m = manifest();
    let rec = record();
    let signed = candidate_cells(&m)
        .unwrap()
        .into_iter()
        .find(|c| c.suite == "artifact-signed-boot")
        .unwrap();
    with_override(&mut m, "candidate", &signed.id, "2026-12-01");
    let mut cells = all_cells(&m, &rec);
    cells.retain(|c| c.evidence.cell != signed.id);
    let g = candidate_gate(&m, &rec, Some(&main_gate(&rec)), &cells, now()).unwrap();
    assert_ne!(g.verdict, Outcome::Passed);
    assert!(g.overrides_applied.is_empty());
}

#[test]
fn release_critical_scenarios_need_every_exact_test_id() {
    let m = manifest();
    let rec = record();
    let mut cells = all_cells(&m, &rec);
    let i = cells
        .iter()
        .position(|c| c.evidence.suite == "guest-strategy")
        .unwrap();
    // Same count, one substituted ID.
    cells[i].evidence.results[3].test = "awman-cli::guest strategy::renamed".into();
    let g = candidate_gate(&m, &rec, Some(&main_gate(&rec)), &cells, now()).unwrap();
    assert_eq!(g.verdict, Outcome::Failed);
    assert!(g
        .release_critical
        .iter()
        .any(|s| s.id == "guest.strategy-matrix" && s.outcome != Outcome::Passed));
}

// ── promotion ────────────────────────────────────────────────────────────────

fn downloads(rec: &ReleaseRecord) -> Vec<Downloaded> {
    rec.artifacts
        .iter()
        .map(|a| Downloaded {
            name: a.name.clone(),
            sha256: a.sha256.clone(),
            attestation_verified: true,
        })
        .collect()
}

fn passing_gate(m: &Manifest, rec: &ReleaseRecord) -> GateEvidence {
    candidate_gate(m, rec, Some(&main_gate(rec)), &all_cells(m, rec), now()).unwrap()
}

fn promo(
    m: &Manifest,
    rec: &ReleaseRecord,
    d: &[Downloaded],
    sums: &str,
    g: &GateEvidence,
) -> Vec<Stop> {
    promotion_findings(
        m,
        rec,
        &release(rec),
        d,
        sums,
        Some((g, "candidate-sha")),
        Some("main-sha"),
        now(),
    )
}

#[test]
fn promotion_requires_every_gate_id_and_identical_bytes() {
    let m = manifest();
    let rec = record();
    let g = passing_gate(&m, &rec);
    assert_eq!(
        required_gates(&m),
        vec!["candidate".to_string(), "main".to_string()]
    );
    assert_eq!(
        promo(&m, &rec, &downloads(&rec), &rec.sha256sums(), &g),
        vec![]
    );
}

#[test]
fn promotion_with_a_missing_or_red_gate_is_refused() {
    let m = manifest();
    let g = passing_gate(&m, &record());
    let mut rec = record();
    rec.gates.retain(|x| x.id != "main");
    let found = promo(&m, &rec, &downloads(&rec), &rec.sha256sums(), &g);
    assert!(
        found
            .iter()
            .any(|s| s.to_string().contains("required gate main is missing")),
        "{found:?}"
    );
    let mut red = record();
    red.gates[1].verdict = "blocked".into();
    assert!(!promo(&m, &red, &downloads(&red), &red.sha256sums(), &g).is_empty());
    let mut other_manifest = record();
    other_manifest.gates[1].manifest_version = "old".into();
    assert!(!promo(
        &m,
        &other_manifest,
        &downloads(&other_manifest),
        &other_manifest.sha256sums(),
        &g
    )
    .is_empty());
    let mut not_validated = record();
    not_validated.state = ReleaseState::CandidateBuilt;
    assert!(!promo(
        &m,
        &not_validated,
        &downloads(&not_validated),
        &not_validated.sha256sums(),
        &g
    )
    .is_empty());
}

#[test]
fn promotion_refuses_changed_bytes_missing_attestation_or_bad_sums() {
    let m = manifest();
    let rec = record();
    let g = passing_gate(&m, &rec);
    let mut changed = downloads(&rec);
    changed[2].sha256 = "0".repeat(64);
    assert!(promo(&m, &rec, &changed, &rec.sha256sums(), &g)[0]
        .to_string()
        .contains("never overridable"));
    let mut unattested = downloads(&rec);
    unattested[0].attestation_verified = false;
    assert!(promo(&m, &rec, &unattested, &rec.sha256sums(), &g)[0]
        .to_string()
        .contains("attestation"));
    let mut missing = downloads(&rec);
    missing.pop();
    assert!(!promo(&m, &rec, &missing, &rec.sha256sums(), &g).is_empty());
    let sums = rec.sha256sums().replacen('a', "b", 1);
    assert!(!promo(&m, &rec, &downloads(&rec), &sums, &g).is_empty());
    let found = promotion_findings(
        &m,
        &rec,
        &release(&rec),
        &downloads(&rec),
        &rec.sha256sums(),
        Some((&g, "tampered")),
        Some("main-sha"),
        now(),
    );
    assert!(found
        .iter()
        .any(|s| s.to_string().contains("does not match its recorded digest")));
}

#[test]
fn tag_is_created_once_and_never_moved() {
    assert_eq!(tag_action(None, COMMIT, "v0.14.0"), Ok(TagAction::Create));
    assert_eq!(
        tag_action(Some(COMMIT), COMMIT, "v0.14.0"),
        Ok(TagAction::Present)
    );
    assert!(
        stop_text(tag_action(Some(&"2".repeat(40)), COMMIT, "v0.14.0")).contains("never moved")
    );
}

#[test]
fn interrupted_promotion_resumes_between_tag_and_publish() {
    let rec = record();
    let rel = release(&rec);
    assert!(stop_text(publish_action(&rel, None, &rec)).contains("tag step first"));
    assert_eq!(
        publish_action(&rel, Some(COMMIT), &rec),
        Ok(PublishAction::Publish)
    );
    let mut published = rel;
    published.draft = false;
    assert_eq!(
        publish_action(&published, Some(COMMIT), &rec),
        Ok(PublishAction::AlreadyPublished)
    );
}

// ── abandon ──────────────────────────────────────────────────────────────────

#[test]
fn abandon_refuses_promoted_or_published_versions() {
    let rec = record();
    let mut published = existing(&rec);
    published.release.draft = false;
    assert!(
        stop_text(abandon_action("v0.14.0", &[published], Some(COMMIT))).contains("never removed")
    );
    assert!(
        stop_text(abandon_action("v0.14.0", &[existing(&rec)], Some(COMMIT))).contains("Resume")
    );
    assert_eq!(
        abandon_action("v0.14.0", &[existing(&rec)], None),
        Ok(AbandonAction::Delete {
            release_id: 500,
            candidate: 1
        })
    );
    assert_eq!(
        abandon_action("v0.14.0", &[], None),
        Ok(AbandonAction::Nothing)
    );
}

// ── state derivation ─────────────────────────────────────────────────────────

fn run(id: u64, title: &str, status: &str, conclusion: Option<&str>) -> RunFact {
    RunFact {
        id,
        title: title.into(),
        status: status.into(),
        conclusion: conclusion.map(str::to_owned),
        created_at: format!("2026-11-11T10:00:{:02}Z", id % 60),
        head_branch: "main".into(),
        event: "workflow_dispatch".into(),
        url: format!("https://example.invalid/runs/{id}"),
    }
}

fn facts() -> Facts {
    Facts {
        version: "v0.14.0".into(),
        ..Default::default()
    }
}

#[test]
fn run_titles_exclude_rehearsals_and_operator_journals() {
    assert_eq!(
        run_commit(&format!("Release v0.14.0 ({COMMIT})"), "v0.14.0"),
        Some(COMMIT.into())
    );
    assert_eq!(
        run_commit(
            &format!("Release v0.14.0 ({COMMIT}) [rehearsal]"),
            "v0.14.0"
        ),
        None
    );
    assert_eq!(
        run_commit(&format!("Release v0.14.0 ({COMMIT}) [journal]"), "v0.14.0"),
        None
    );
    assert_eq!(
        run_commit(&format!("Release v0.14.1 ({COMMIT})"), "v0.14.0"),
        None
    );
}

#[test]
fn state_is_derived_from_store_facts() {
    assert_eq!(derive(&facts()).state, ReleaseState::None);

    let mut f = facts();
    f.runs = vec![
        run(
            1,
            &format!("Release v0.14.0 ({COMMIT})"),
            "in_progress",
            None,
        ),
        run(
            2,
            &format!("Release v0.14.0 ({COMMIT}) [journal]"),
            "completed",
            Some("success"),
        ),
    ];
    let s = derive(&f);
    assert_eq!(
        (s.state, s.run_id, s.commit.as_deref()),
        (ReleaseState::Prepared, Some(1), Some(COMMIT))
    );

    let mut rec = record();
    rec.state = ReleaseState::CandidateBuilt;
    f.existing = vec![existing(&rec)];
    assert_eq!(derive(&f).state, ReleaseState::CandidateBuilt);
    f.existing = vec![existing(&record())];
    let s = derive(&f);
    assert_eq!((s.state, s.candidate), (ReleaseState::Validated, Some(1)));

    let mut published = existing(&record());
    published.release.draft = false;
    f.existing = vec![published.clone()];
    f.tag = Some(COMMIT.into());
    let s = derive(&f);
    assert_eq!(s.state, ReleaseState::Promoted, "no receipt yet");
    assert!(s.tag_on_origin);
    assert!(s.failing_gates.iter().any(|g| g.gate_id == "verify"));

    let rec = record();
    let mut receipt = Receipt {
        schema: 1,
        version: rec.version.clone(),
        candidate: 1,
        release_id: 500,
        tag_commit: COMMIT.into(),
        assets: rec
            .artifacts
            .iter()
            .map(|a| (a.name.clone(), a.sha256.clone()))
            .collect(),
        targets_verified: rec.artifacts.iter().map(|a| a.name.clone()).collect(),
        run_id: 8,
        verified_at: now(),
    };
    f.receipt = Some(receipt.clone());
    assert_eq!(derive(&f).state, ReleaseState::Verified);
    receipt.release_id = 501;
    f.receipt = Some(receipt);
    assert_eq!(
        derive(&f).state,
        ReleaseState::Promoted,
        "a receipt for another release proves nothing"
    );
}

#[test]
fn abandoned_and_failed_runs_are_reported_with_reasons() {
    let mut f = facts();
    f.runs = vec![run(
        3,
        &format!("Release v0.14.0 ({COMMIT})"),
        "completed",
        Some("failure"),
    )];
    let s = derive(&f);
    assert_eq!(s.state, ReleaseState::Prepared);
    assert_eq!(s.failing_gates[0].outcome, "failed");
    f.runs[0].conclusion = Some("cancelled".into());
    assert_eq!(derive(&f).failing_gates[0].outcome, "blocked");
    f.recorded = vec![1, 2];
    let s = derive(&f);
    assert_eq!((s.state, s.candidate), (ReleaseState::Abandoned, Some(2)));
}

#[test]
fn draft_without_complete_assets_is_only_prepared() {
    let mut f = facts();
    let mut e = existing(&record());
    e.release.assets.retain(|a| a.name != "awman-linux-amd64");
    f.existing = vec![e];
    assert_eq!(derive(&f).state, ReleaseState::Prepared);
    let mut promoted_draft = existing(&record());
    promoted_draft.record.as_mut().unwrap().state = ReleaseState::Promoted;
    f.existing = vec![promoted_draft];
    assert_eq!(
        derive(&f).state,
        ReleaseState::Validated,
        "tag/publish interrupted: resume promotion"
    );
}
