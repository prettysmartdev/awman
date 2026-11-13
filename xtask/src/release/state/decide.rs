//! Every evidence and promotion decision of the release pipeline, as pure
//! functions over facts read from the store. The subcommands only gather
//! facts, call one of these, and carry out the answer; the unit tests at the
//! bottom cover each recovery case of WI 5.5 and design 12.4.

use super::args::Stop;
use super::hub::{ReleaseFact, RunFact};
use super::inputs::Policy;
use super::record::{
    candidate_of_name, parse_sha256sums, Identity, Receipt, ReleaseRecord, PRODUCTION, STATE_ASSET,
};
use super::{FailingGate, ReleaseState, ReleaseStatus};
use crate::evidence::schema::{Evidence, GateCell, GateEvidence, Outcome, ScenarioResult};
use crate::manifest::{self, Manifest};
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::BTreeSet;

/// A release on the store together with its parsed `release-state.json`.
#[derive(Debug, Clone)]
pub struct Existing {
    pub release: ReleaseFact,
    pub record: Option<ReleaseRecord>,
}

impl Existing {
    pub fn candidate(&self) -> Option<u32> {
        self.record
            .as_ref()
            .map(|r| r.candidate)
            .or_else(|| candidate_of_name(&self.release.name))
    }
}

/// Production releases of `version`: tag name equal to the version. Rehearsal
/// drafts carry a namespaced tag and are never seen here.
pub fn production(version: &str, all: Vec<Existing>) -> Vec<Existing> {
    all.into_iter()
        .filter(|e| e.release.tag == version)
        .collect()
}

// ── 1. Preflight: which candidate this run is ────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Build a new candidate with this number; older drafts are superseded at
    /// assembly, under the publication lock.
    New { candidate: u32 },
    /// The existing draft has the same identity and complete bytes: validate
    /// or promote it without building anything.
    Resume {
        candidate: u32,
        release_id: u64,
        state: ReleaseState,
    },
    /// The version is published at this commit; only verification can remain.
    Published {
        release_id: u64,
        candidate: Option<u32>,
    },
}

/// Decide what a candidate run for `id` does. `max_recorded` is the highest
/// candidate number found in retained record artifacts (abandoned or
/// superseded candidates keep their number forever).
pub fn resolve(
    id: &Identity,
    existing: &[Existing],
    max_recorded: u32,
    tag_commit: Option<&str>,
) -> Result<Resolution, Stop> {
    let version = &id.version;
    if let Some(p) = existing.iter().find(|e| !e.release.draft) {
        return match tag_commit {
            Some(c) if c == id.commit => Ok(Resolution::Published {
                release_id: p.release.id,
                candidate: p.candidate(),
            }),
            Some(c) => Err(Stop::Failed(format!(
                "{version} is already published from {c}; a published tag is never moved or \
                 reused. Release a new patch version"
            ))),
            None => Err(Stop::Blocked(format!(
                "{version} is published but its tag is not readable on origin; check the \
                 repository's tags and rerun"
            ))),
        };
    }
    if let Some(c) = tag_commit {
        if c != id.commit {
            return Err(Stop::Failed(format!(
                "tag {version} already exists on origin at {c}, not at {}; tags are never \
                 moved. Delete the stray tag only if nothing was published from it",
                id.commit
            )));
        }
    }
    let drafts: Vec<&Existing> = existing.iter().filter(|e| e.release.draft).collect();
    let highest = drafts
        .iter()
        .filter_map(|e| e.candidate())
        .chain([max_recorded])
        .max()
        .unwrap_or(0);
    if let [only] = drafts.as_slice() {
        if let Some(rec) = &only.record {
            let complete = matches!(
                rec.state,
                ReleaseState::CandidateBuilt | ReleaseState::Validated | ReleaseState::Promoted
            ) && rec
                .artifacts
                .iter()
                .all(|a| only.release.asset(&a.name).is_some())
                && only.release.asset(STATE_ASSET).is_some();
            if rec.identity() == *id && complete {
                return Ok(Resolution::Resume {
                    candidate: rec.candidate,
                    release_id: only.release.id,
                    state: rec.state,
                });
            }
        }
    }
    Ok(Resolution::New {
        candidate: highest + 1,
    })
}

// ── 2. Assembly: compare-and-check under the publication lock ────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assembly {
    /// Create the draft for this candidate, after deleting `supersede`.
    Create { supersede: Vec<u64> },
    /// The draft was created by this same run (a rerun of assembly): finish it.
    Continue {
        release_id: u64,
        supersede: Vec<u64>,
    },
}

/// Marker written in a draft's body when the draft is created, so a rerun of
/// the same run recognizes its own partial draft before any state is uploaded.
pub fn run_marker(run_id: u64) -> String {
    format!("Release candidate in preparation by run {run_id}.")
}

pub fn assembly(
    id: &Identity,
    candidate: u32,
    run_id: u64,
    existing: &[Existing],
) -> Result<Assembly, Stop> {
    if existing.iter().any(|e| !e.release.draft) {
        return Err(Stop::Failed(format!(
            "{} was published while this candidate was being assembled; nothing was changed",
            id.version
        )));
    }
    let mut ours = None;
    let mut supersede = Vec::new();
    for e in existing {
        let n = e.candidate().unwrap_or(0);
        let mine = e.release.body.contains(&run_marker(run_id))
            && e.release.target_commitish == id.commit
            && e.record.as_ref().is_none_or(|r| r.identity() == *id);
        if n == candidate && mine {
            ours = Some(e.release.id);
        } else if n >= candidate {
            return Err(Stop::Blocked(format!(
                "candidate {n} for {} was allocated concurrently by another run; nothing was \
                 published. Rerun the candidate workflow to allocate the next number",
                id.version
            )));
        } else {
            supersede.push(e.release.id);
        }
    }
    Ok(match ours {
        Some(release_id) => Assembly::Continue {
            release_id,
            supersede,
        },
        None => Assembly::Create { supersede },
    })
}

/// Assets this rerun still has to upload, refusing changed bytes. A recorded
/// asset whose bytes differ is lost: it cannot be rebuilt under this identity.
pub fn missing_uploads(release: &ReleaseFact, rec: &ReleaseRecord) -> Result<Vec<String>, Stop> {
    let mut out = Vec::new();
    for a in &rec.artifacts {
        match release.asset(&a.name) {
            None => out.push(a.name.clone()),
            Some(fact) => {
                let size_ok = fact.size == a.bytes;
                let digest_ok = fact
                    .digest
                    .as_deref()
                    .is_none_or(|d| d == format!("sha256:{}", a.sha256));
                if !size_ok || !digest_ok {
                    return Err(Stop::Failed(format!(
                        "draft asset {} differs from the recorded bytes; lost bytes are never \
                         rebuilt under the same candidate. Abandon it and start a new candidate",
                        a.name
                    )));
                }
            }
        }
    }
    Ok(out)
}

// ── 3. Evidence validity: freshness, ancestry and current policy ─────────────

/// What is true now, from the controller and the protected main tip.
#[derive(Debug, Clone)]
pub struct Current {
    pub now: DateTime<Utc>,
    pub main_contains_commit: bool,
    /// Manifest, notes and entitlement digests on the main tip.
    pub policy: Policy,
}

/// Why a candidate's evidence can no longer be used. Empty means valid.
pub fn evidence_findings(
    rec: &ReleaseRecord,
    main_manifest: &Manifest,
    cur: &Current,
    for_publication: bool,
) -> Vec<Stop> {
    let mut out = Vec::new();
    let v = &rec.version;
    if !cur.main_contains_commit {
        out.push(Stop::Failed(format!(
            "commit {} is no longer the tip or an ancestor of main; candidate {} of {v} is void",
            rec.commit, rec.candidate
        )));
    }
    let recorded = Policy::of(&rec.inputs);
    if rec.manifest_version != recorded.manifest_version
        || rec.inputs.digest().ok().as_deref() != Some(rec.input_digest.as_str())
    {
        out.push(Stop::Failed(
            "release-state.json is internally inconsistent".into(),
        ));
    }
    for a in &rec.artifacts {
        if rec.inputs.target_digest(&a.name).ok().as_deref() != Some(a.input_digest.as_str()) {
            out.push(Stop::Failed(format!(
                "{} was built from inputs other than this candidate's",
                a.name
            )));
        }
    }
    if cur.policy.manifest_version != recorded.manifest_version {
        out.push(Stop::Failed(format!(
            "ci/gates.toml changed on main since candidate {} was built; start a new candidate",
            rec.candidate
        )));
    }
    if cur.policy.release_notes_sha256 != recorded.release_notes_sha256 {
        out.push(Stop::Failed(format!(
            "docs/releases/{v}.md changed on main since candidate {} was built; start a new \
             candidate so the published notes are the validated notes",
            rec.candidate
        )));
    }
    if cur.policy.entitlements_sha256 != recorded.entitlements_sha256 {
        out.push(Stop::Failed(format!(
            "{} changed on main since candidate {} was signed; start a new candidate",
            super::inputs::ENTITLEMENTS,
            rec.candidate
        )));
    }
    if cur.now >= rec.expires_at {
        out.push(Stop::Failed(format!(
            "candidate {} of {v} expired at {}; evidence age is never reset",
            rec.candidate, rec.expires_at
        )));
    }
    for g in &rec.gates {
        if cur.now >= g.expires_at {
            out.push(Stop::Failed(format!(
                "gate {} evidence expired at {}",
                g.id, g.expires_at
            )));
        }
        for o in &g.overrides_applied {
            if let Err(e) = override_allowed(main_manifest, &g.id, o, cur.now.date_naive()) {
                out.push(Stop::Failed(format!(
                    "override for {o} no longer authorizes gate {}: {e}",
                    g.id
                )));
            }
        }
    }
    if let Err(e) = manifest::validate::expiry(main_manifest, cur.now.date_naive()) {
        out.push(Stop::Failed(format!("{e:#}; affected evidence is invalid")));
    }
    if for_publication && (rec.dry_run || rec.namespace != PRODUCTION) {
        out.push(Stop::Failed(
            "rehearsal evidence can never authorize a real promotion".into(),
        ));
    }
    out
}

// ── 4. Overrides ──────────────────────────────────────────────────────────────

/// Whether a committed manifest override may make `cell` count for `gate`.
/// Never for hardware, guest, artifact integrity, signed boot, upgrade or a
/// `not_overridable` scenario, and never once expired.
pub fn override_allowed(
    m: &Manifest,
    gate: &str,
    cell: &str,
    today: NaiveDate,
) -> Result<(), String> {
    let o = m
        .overrides
        .iter()
        .find(|o| o.gate == gate && o.cell == cell)
        .ok_or_else(|| format!("no committed override for {gate}/{cell}"))?;
    let expires = NaiveDate::parse_from_str(&o.expires, "%Y-%m-%d")
        .map_err(|_| format!("override {cell} has an invalid expiry"))?;
    if expires <= today {
        return Err(format!("override {cell} expired on {}", o.expires));
    }
    if o.reason.trim().is_empty() || o.approved_by.trim().is_empty() {
        return Err(format!("override {cell} needs a reason and an approver"));
    }
    let suite_id = cell.split('@').next().unwrap_or("");
    let build_part = cell.split('@').nth(1).unwrap_or("");
    let build_id = build_part.split('+').next().unwrap_or("");
    let platform = build_part
        .split_once('+')
        .map(|(_, p)| p.to_owned())
        .or_else(|| m.builds.get(build_id).map(|b| b.platform.clone()))
        .unwrap_or_default();
    let Some(suite) = m.suites.get(suite_id) else {
        return Err(format!(
            "{cell}: build and artifact integrity cells cannot be overridden"
        ));
    };
    if m.platforms
        .get(&platform)
        .is_none_or(|p| p.trust != "hosted")
    {
        return Err(format!(
            "{cell}: required hardware coverage cannot be overridden"
        ));
    }
    if ["guest", "artifact"].contains(&suite.tier.as_str()) {
        return Err(format!(
            "{cell}: guest, signed boot, upgrade and artifact integrity cannot be overridden"
        ));
    }
    if m.release_critical
        .iter()
        .any(|s| s.suite == suite_id && s.not_overridable)
    {
        return Err(format!(
            "{cell}: release-critical scenario cannot be overridden"
        ));
    }
    Ok(())
}

// ── 5. The candidate gate ─────────────────────────────────────────────────────

/// One downloaded cell record and whether the controller authenticated its
/// producer (artifact of this repository's candidate run on main).
#[derive(Debug, Clone)]
pub struct CellRecord {
    pub evidence: Evidence,
    pub sha256: String,
    pub authenticated: bool,
}

/// The main gate evidence for the commit and whether it is authenticated
/// (artifact of a successful protected-main `ci.yml` run for this commit).
#[derive(Debug, Clone)]
pub struct MainGate {
    pub evidence: GateEvidence,
    pub sha256: String,
    pub authenticated: bool,
}

/// Cells the candidate gate adds to `main`: artifact tier on every target
/// and guest tier on the release Mac.
pub fn candidate_cells(m: &Manifest) -> anyhow::Result<Vec<manifest::Cell>> {
    let main: BTreeSet<String> = manifest::validate::cells(m, "main")?
        .into_iter()
        .map(|c| c.id)
        .collect();
    Ok(manifest::validate::cells(m, "candidate")?
        .into_iter()
        .filter(|c| !main.contains(&c.id))
        .collect())
}

fn worst(a: Outcome, b: Outcome) -> Outcome {
    let rank = |o: Outcome| match o {
        Outcome::Passed => 0,
        Outcome::Blocked => 1,
        _ => 2,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// Evaluate one cell from its records. Capability loss or a blocked phase is
/// infrastructure (`Blocked`); anything else that is not an exact pass of a
/// bound, fresh record is `Failed`. A skip never passes a required cell.
fn cell_outcome(
    m: &Manifest,
    rec: &ReleaseRecord,
    cell: &manifest::Cell,
    rs: &[&CellRecord],
    now: DateTime<Utc>,
    errors: &mut Vec<String>,
) -> Outcome {
    let mut seen: Vec<Outcome> = Vec::new();
    let mut fail = |o: Outcome, why: String, errors: &mut Vec<String>| {
        errors.push(format!("{}: {why}", cell.id));
        seen.push(o);
    };
    if rs.is_empty() {
        fail(
            Outcome::Blocked,
            "no evidence (cell did not run or was lost)".into(),
            errors,
        );
    }
    let shard_count = rs.first().map(|r| r.evidence.shard.of).unwrap_or(0);
    let shard_ids: BTreeSet<_> = rs.iter().map(|r| r.evidence.shard.index).collect();
    if !rs.is_empty()
        && (shard_ids.len() != shard_count as usize
            || shard_ids.len() != rs.len()
            || rs.iter().any(|r| r.evidence.shard.of != shard_count))
    {
        fail(Outcome::Failed, "missing/duplicate shards".into(), errors);
    }
    let asset = m.builds.get(&cell.build).and_then(|b| b.asset.clone());
    for r in rs {
        let e = &r.evidence;
        for error in crate::evidence::aggregate::record_findings(m, e) {
            fail(Outcome::Failed, error, errors);
        }
        if e.plan_sha256 != rec.compute_candidate_digest().unwrap_or_default()
            || e.platform != cell.platform
        {
            fail(
                Outcome::Failed,
                "candidate plan/platform mismatch".into(),
                errors,
            );
        }
        if !r.authenticated {
            fail(
                Outcome::Failed,
                "producer is not an authenticated candidate job".into(),
                errors,
            );
        }
        if e.gate != "candidate"
            || e.commit != rec.commit
            || e.manifest_version != rec.manifest_version
        {
            fail(
                Outcome::Failed,
                "stale or substituted evidence (commit/manifest/gate)".into(),
                errors,
            );
        }
        if e.suite != cell.suite || e.build != cell.build {
            fail(
                Outcome::Failed,
                "evidence names another suite or build".into(),
                errors,
            );
        }
        if let Some(asset) = &asset {
            let want = rec.artifact(asset).map(|a| a.sha256.clone());
            let bound = e
                .artifacts
                .iter()
                .any(|a| Some(&a.sha256) == want.as_ref() && &a.name == asset);
            if !bound {
                fail(
                    Outcome::Failed,
                    format!("evidence is not bound to the candidate's {asset} bytes"),
                    errors,
                );
            }
        }
        if e.expires_at <= now {
            fail(Outcome::Failed, "evidence expired".into(), errors);
        }
        if e.results.is_empty() {
            fail(
                Outcome::Failed,
                "required suite reported no tests".into(),
                errors,
            );
        }
        for phase in ["setup", "run", "sweep", "upload"] {
            match e.phases.get(phase) {
                Some(Outcome::Passed) => {}
                Some(Outcome::Blocked) => {
                    fail(Outcome::Blocked, format!("phase {phase} blocked"), errors)
                }
                _ => fail(
                    Outcome::Failed,
                    format!("phase {phase} missing or failed"),
                    errors,
                ),
            }
        }
        for t in &e.results {
            if !t.capabilities_lost.is_empty() {
                fail(
                    Outcome::Blocked,
                    format!(
                        "{} lost {} mid-run: infrastructure-blocked, not a pass or a skip",
                        t.test,
                        t.capabilities_lost.join(", ")
                    ),
                    errors,
                );
                continue;
            }
            match t.outcome {
                Outcome::Passed => {}
                Outcome::Blocked => fail(
                    Outcome::Blocked,
                    format!(
                        "{} blocked: {}",
                        t.test,
                        t.reason.clone().unwrap_or_default()
                    ),
                    errors,
                ),
                other => fail(
                    Outcome::Failed,
                    format!("required test {} is {other:?}", t.test),
                    errors,
                ),
            }
        }
        if e.outcome == Outcome::Blocked {
            fail(Outcome::Blocked, "suite reported blocked".into(), errors);
        } else if e.outcome != Outcome::Passed {
            fail(
                Outcome::Failed,
                format!("suite reported {:?}", e.outcome),
                errors,
            );
        }
    }
    seen.into_iter().fold(Outcome::Passed, worst)
}

/// Build the candidate gate evidence: the main gate for the commit plus every
/// candidate cell against the candidate's exact bytes, release-critical
/// scenarios by exact test ID, and only committed, permitted overrides.
pub fn candidate_gate(
    m: &Manifest,
    rec: &ReleaseRecord,
    main: Option<&MainGate>,
    records: &[CellRecord],
    now: DateTime<Utc>,
) -> anyhow::Result<GateEvidence> {
    let mut errors = Vec::new();
    let mut missing = Vec::new();
    let mut cells = Vec::new();
    let mut overrides = Vec::new();
    let mut verdict = Outcome::Passed;
    let mut expires = rec.expires_at;
    match main {
        None => {
            missing.push("gate:main".into());
            verdict = worst(verdict, Outcome::Blocked);
        }
        Some(g) => {
            let e = &g.evidence;
            let mut ok = true;
            if !g.authenticated {
                errors
                    .push("gate:main: not from an authenticated protected-main ci.yml run".into());
                ok = false;
            }
            if e.gate != "main"
                || e.commit != rec.commit
                || e.manifest_version != rec.manifest_version
            {
                errors.push("gate:main: evidence is for another commit, gate or manifest".into());
                ok = false;
            }
            if e.expires_at.is_none_or(|x| x <= now) {
                errors.push("gate:main: evidence expired".into());
                ok = false;
            }
            let o = if !ok {
                Outcome::Failed
            } else if e.verdict == Outcome::Passed {
                Outcome::Passed
            } else {
                errors.push(format!("gate:main: verdict {:?}", e.verdict));
                if e.verdict == Outcome::Blocked {
                    Outcome::Blocked
                } else {
                    Outcome::Failed
                }
            };
            verdict = worst(verdict, o);
            if let Some(x) = e.expires_at {
                expires = expires.min(x);
            }
            cells.push(GateCell {
                cell: "gate:main".into(),
                outcome: o,
                evidence_sha256: vec![g.sha256.clone()],
                counts: Default::default(),
            });
        }
    }
    let wanted = candidate_cells(m)?;
    let known: BTreeSet<&str> = wanted.iter().map(|c| c.id.as_str()).collect();
    for r in records {
        if !known.contains(r.evidence.cell.as_str()) {
            errors.push(format!("unknown evidence cell {}", r.evidence.cell));
            verdict = worst(verdict, Outcome::Failed);
        }
    }
    for cell in &wanted {
        let rs: Vec<&CellRecord> = records
            .iter()
            .filter(|r| r.evidence.cell == cell.id)
            .collect();
        let mut cell_errors = Vec::new();
        let mut o = cell_outcome(m, rec, cell, &rs, now, &mut cell_errors);
        if o != Outcome::Passed
            && override_allowed(m, "candidate", &cell.id, now.date_naive()).is_ok()
        {
            overrides.push(cell.id.clone());
            o = Outcome::Passed;
        } else {
            errors.extend(cell_errors);
        }
        if rs.is_empty() && o != Outcome::Passed {
            missing.push(cell.id.clone());
        }
        for r in &rs {
            expires = expires.min(r.evidence.expires_at);
        }
        verdict = worst(verdict, o);
        let results: Vec<_> = rs.iter().flat_map(|r| r.evidence.results.clone()).collect();
        cells.push(GateCell {
            cell: cell.id.clone(),
            outcome: o,
            evidence_sha256: rs.iter().map(|r| r.sha256.clone()).collect(),
            counts: crate::evidence::junit::counts(results.len(), &results),
        });
    }
    let mut scenarios = Vec::new();
    for s in m
        .release_critical
        .iter()
        .filter(|s| s.gates.iter().any(|g| g == "candidate"))
    {
        let passed = s.tests.iter().all(|t| {
            let applicable: Vec<_> = wanted.iter().filter(|c| c.suite == s.suite).collect();
            !applicable.is_empty()
                && applicable.iter().all(|cell| {
                    records.iter().any(|r| {
                        r.authenticated
                            && r.evidence.cell == cell.id
                            && r.evidence.results.iter().any(|x| {
                                &x.test == t
                                    && x.outcome == Outcome::Passed
                                    && x.capabilities_lost.is_empty()
                            })
                    })
                })
        });
        if !passed {
            errors.push(format!(
                "release-critical scenario {} missing or non-green",
                s.id
            ));
            let blocked = cells
                .iter()
                .filter(|c| c.cell.starts_with(&format!("{}@", s.suite)))
                .all(|c| c.outcome == Outcome::Blocked);
            verdict = worst(
                verdict,
                if blocked {
                    Outcome::Blocked
                } else {
                    Outcome::Failed
                },
            );
        }
        scenarios.push(ScenarioResult {
            id: s.id.clone(),
            outcome: if passed {
                Outcome::Passed
            } else {
                Outcome::Failed
            },
        });
    }
    for o in &m.overrides {
        if let Ok(d) = NaiveDate::parse_from_str(&o.expires, "%Y-%m-%d") {
            expires = expires.min(d.and_hms_opt(0, 0, 0).unwrap().and_utc());
        }
    }
    for q in &m.quarantine {
        if let Ok(d) = NaiveDate::parse_from_str(&q.expires, "%Y-%m-%d") {
            expires = expires.min(d.and_hms_opt(0, 0, 0).unwrap().and_utc());
        }
    }
    if let Err(e) = manifest::validate::expiry(m, now.date_naive()) {
        errors.push(format!("{e:#}"));
        verdict = worst(verdict, Outcome::Failed);
    }
    Ok(GateEvidence {
        schema: 1,
        kind: "gate".into(),
        gate: "candidate".into(),
        commit: rec.commit.clone(),
        manifest_version: rec.manifest_version.clone(),
        plan_sha256: rec.candidate_digest.clone().unwrap_or_default(),
        verdict,
        cells,
        missing,
        errors,
        release_critical: scenarios,
        quarantine: m
            .quarantine
            .iter()
            .map(|q| format!("{} expires {}", q.test, q.expires))
            .collect(),
        overrides_applied: overrides,
        queue_ms: 0,
        execution_ms: 0,
        p95_budget_minutes: m.gates.get("candidate").and_then(|g| g.budget_minutes),
        generated_at: now,
        expires_at: Some(expires),
    })
}

/// The failing gates `release status` prints, from a gate evidence file.
pub fn failing_gates(g: &GateEvidence) -> Vec<FailingGate> {
    let mut out: Vec<FailingGate> = g
        .cells
        .iter()
        .filter(|c| c.outcome != Outcome::Passed)
        .map(|c| FailingGate {
            gate_id: c.cell.clone(),
            outcome: if c.outcome == Outcome::Blocked {
                "blocked"
            } else {
                "failed"
            }
            .into(),
            reason: g
                .errors
                .iter()
                .find(|e| e.starts_with(&format!("{}:", c.cell)))
                .cloned()
                .unwrap_or_else(|| "no evidence".into()),
        })
        .collect();
    for s in g
        .release_critical
        .iter()
        .filter(|s| s.outcome != Outcome::Passed)
    {
        out.push(FailingGate {
            gate_id: s.id.clone(),
            outcome: if g.verdict == Outcome::Blocked {
                "blocked"
            } else {
                "failed"
            }
            .into(),
            reason: "release-critical scenario missing or non-green".into(),
        });
    }
    out
}

// ── 6. Promotion ──────────────────────────────────────────────────────────────

/// One asset as re-downloaded at promotion.
#[derive(Debug, Clone)]
pub struct Downloaded {
    pub name: String,
    pub sha256: String,
    pub attestation_verified: bool,
}

/// Required gate IDs for promotion: the gate promote requires plus every gate
/// it includes (`candidate` includes `main`).
pub fn required_gates(m: &Manifest) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = m
        .gates
        .get("promote")
        .and_then(|g| g.requires_gate.clone())
        .into_iter()
        .collect();
    while let Some(g) = stack.pop() {
        if out.contains(&g) {
            continue;
        }
        if let Some(gate) = m.gates.get(&g) {
            stack.extend(gate.includes.iter().filter(|i| *i != "pr").cloned());
        }
        out.push(g);
    }
    out
}

/// Everything promotion checks before tagging. Empty means promotable.
#[allow(clippy::too_many_arguments)]
pub fn promotion_findings(
    m: &Manifest,
    rec: &ReleaseRecord,
    release: &ReleaseFact,
    downloaded: &[Downloaded],
    sha256sums: &str,
    candidate_gate: Option<(&GateEvidence, &str)>,
    main_gate_sha: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<Stop> {
    let mut out = Vec::new();
    if !matches!(rec.state, ReleaseState::Validated | ReleaseState::Promoted) {
        out.push(Stop::Failed(format!(
            "candidate {} is {:?}, not validated",
            rec.candidate, rec.state
        )));
    }
    let names: Vec<&String> = rec.artifacts.iter().map(|a| &a.name).collect();
    if names != m.release.assets.iter().collect::<Vec<_>>() {
        out.push(Stop::Failed(format!(
            "candidate assets {names:?} differ from release.assets {:?}",
            m.release.assets
        )));
    }
    if release.target_commitish != rec.commit {
        out.push(Stop::Failed(
            "draft targets another commit than its record".into(),
        ));
    }
    for a in &rec.artifacts {
        match downloaded.iter().find(|d| d.name == a.name) {
            None => out.push(Stop::Failed(format!("{} was not re-downloaded", a.name))),
            Some(d) if d.sha256 != a.sha256 => out.push(Stop::Failed(format!(
                "{} downloaded as {} but the evidence names {}; artifact integrity is never \
                 overridable",
                a.name, d.sha256, a.sha256
            ))),
            Some(d) if !d.attestation_verified => out.push(Stop::Failed(format!(
                "{} has no verified provenance attestation from the candidate workflow",
                a.name
            ))),
            Some(_) => {}
        }
        if let Some(f) = release.asset(&a.name) {
            if f.digest
                .as_deref()
                .is_some_and(|d| d != format!("sha256:{}", a.sha256))
            {
                out.push(Stop::Failed(format!(
                    "service digest of {} disagrees",
                    a.name
                )));
            }
        }
    }
    match parse_sha256sums(sha256sums) {
        Ok(sums) => {
            let want: Vec<(String, String)> = rec
                .artifacts
                .iter()
                .map(|a| (a.sha256.clone(), a.name.clone()))
                .collect();
            if sums != want {
                out.push(Stop::Failed(
                    "SHA256SUMS does not match the evidence".into(),
                ));
            }
        }
        Err(e) => out.push(Stop::Failed(format!("{e:#}"))),
    }
    for id in required_gates(m) {
        let Some(g) = rec.gate(&id) else {
            out.push(Stop::Failed(format!(
                "required gate {id} is missing for {} at {}; promotion refused",
                rec.commit, rec.manifest_version
            )));
            continue;
        };
        if g.verdict != "passed"
            || g.commit != rec.commit
            || g.manifest_version != rec.manifest_version
        {
            out.push(Stop::Failed(format!(
                "required gate {id} is {} for {} under manifest {}; promotion refused",
                g.verdict, g.commit, g.manifest_version
            )));
        }
        if g.expires_at <= now {
            out.push(Stop::Failed(format!(
                "required gate {id} expired at {}",
                g.expires_at
            )));
        }
        let downloaded_sha = if id == "candidate" {
            candidate_gate.map(|(_, s)| s)
        } else if id == "main" {
            main_gate_sha
        } else {
            None
        };
        if downloaded_sha != Some(g.gate_evidence_sha256.as_str()) {
            out.push(Stop::Failed(format!(
                "gate {id} evidence on the draft does not match its recorded digest"
            )));
        }
    }
    if let Some((g, _)) = candidate_gate {
        if g.plan_sha256 != rec.compute_candidate_digest().unwrap_or_default()
            || g.verdict != Outcome::Passed
            || g.commit != rec.commit
            || g.manifest_version != rec.manifest_version
        {
            out.push(Stop::Failed(
                "candidate gate evidence is not a pass for this candidate".into(),
            ));
        }
        if let Ok(wanted) = candidate_cells(m) {
            for c in wanted {
                if !g
                    .cells
                    .iter()
                    .any(|x| x.cell == c.id && x.outcome == Outcome::Passed)
                {
                    out.push(Stop::Failed(format!(
                        "required cell {} is not passed",
                        c.id
                    )));
                }
            }
        }
        for s in m
            .release_critical
            .iter()
            .filter(|s| s.gates.iter().any(|x| x == "candidate"))
        {
            if !g
                .release_critical
                .iter()
                .any(|r| r.id == s.id && r.outcome == Outcome::Passed)
            {
                out.push(Stop::Failed(format!(
                    "release-critical scenario {} not passed",
                    s.id
                )));
            }
        }
        for o in &g.overrides_applied {
            if let Err(e) = override_allowed(m, "candidate", o, now.date_naive()) {
                out.push(Stop::Failed(format!("override refused: {e}")));
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagAction {
    Create,
    /// Already at the candidate commit: an interrupted promotion resumes.
    Present,
}

pub fn tag_action(existing: Option<&str>, commit: &str, version: &str) -> Result<TagAction, Stop> {
    match existing {
        None => Ok(TagAction::Create),
        Some(c) if c == commit => Ok(TagAction::Present),
        Some(c) => Err(Stop::Failed(format!(
            "tag {version} exists on origin at {c}, not at the candidate commit {commit}; a \
             tag is never moved"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishAction {
    Publish,
    AlreadyPublished,
}

pub fn publish_action(
    release: &ReleaseFact,
    tag_commit: Option<&str>,
    rec: &ReleaseRecord,
) -> Result<PublishAction, Stop> {
    if tag_commit != Some(rec.commit.as_str()) {
        return Err(Stop::Failed(format!(
            "tag {} is not at {}; run the tag step first",
            rec.version, rec.commit
        )));
    }
    if !release.draft {
        return Ok(PublishAction::AlreadyPublished);
    }
    Ok(PublishAction::Publish)
}

// ── 7. Abandon ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbandonAction {
    Delete { release_id: u64, candidate: u32 },
    Nothing,
}

pub fn abandon_action(
    version: &str,
    existing: &[Existing],
    tag: Option<&str>,
) -> Result<AbandonAction, Stop> {
    if existing.iter().any(|e| !e.release.draft) {
        return Err(Stop::Failed(format!(
            "{version} is published; a published release is never removed or moved. Publish \
             a fixed patch version and mark this one superseded in its notes"
        )));
    }
    if let Some(c) = tag {
        return Err(Stop::Failed(format!(
            "tag {version} is already on origin at {c}: promotion is in progress. Resume it \
             with `make release VERSION={version}` instead of abandoning"
        )));
    }
    let newest = existing.iter().max_by_key(|e| e.candidate().unwrap_or(0));
    Ok(match newest {
        Some(e) => AbandonAction::Delete {
            release_id: e.release.id,
            candidate: e.candidate().unwrap_or(0),
        },
        None => AbandonAction::Nothing,
    })
}

// ── 8. State derivation (`state::query`) ─────────────────────────────────────

/// Commit in a candidate run title `Release vX.Y.Z (<sha>)…`; `None` for
/// rehearsals and record-only operator journals, which never select a run.
pub fn run_commit(title: &str, version: &str) -> Option<String> {
    let rest = title.strip_prefix(&format!("Release {version} ("))?;
    let (commit, tail) = rest.split_once(')')?;
    if tail.contains("[rehearsal]") || tail.contains("[journal]") {
        return None;
    }
    Some(commit.to_owned())
}

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub version: String,
    pub existing: Vec<Existing>,
    pub tag: Option<String>,
    pub runs: Vec<RunFact>,
    /// Candidate numbers of retained record artifacts.
    pub recorded: Vec<u32>,
    pub receipt: Option<Receipt>,
}

/// The receipt proves verification only if it binds this exact publication.
pub fn receipt_matches(r: &Receipt, release: &ReleaseFact, rec: &ReleaseRecord, tag: &str) -> bool {
    let want: Vec<(String, String)> = rec
        .artifacts
        .iter()
        .map(|a| (a.name.clone(), a.sha256.clone()))
        .collect();
    let mut targets = r.targets_verified.clone();
    targets.sort();
    let mut assets = rec
        .artifacts
        .iter()
        .map(|a| a.name.clone())
        .collect::<Vec<_>>();
    assets.sort();
    r.schema == 1
        && r.version == rec.version
        && r.candidate == rec.candidate
        && r.release_id == release.id
        && r.tag_commit == tag
        && tag == rec.commit
        && r.assets == want
        && targets == assets
}

pub fn derive(f: &Facts) -> ReleaseStatus {
    let mut st = ReleaseStatus {
        version: f.version.clone(),
        state: ReleaseState::None,
        candidate: None,
        commit: None,
        manifest_version: None,
        run_id: None,
        failing_gates: Vec::new(),
        evidence_links: Vec::new(),
        tag_on_origin: f.tag.is_some(),
    };
    let newest_run = f
        .runs
        .iter()
        .filter(|r| run_commit(&r.title, &f.version).is_some())
        .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    if let Some(r) = newest_run {
        st.run_id = Some(r.id);
        st.evidence_links.push(r.url.clone());
    }
    let take = |st: &mut ReleaseStatus, e: &Existing| {
        st.candidate = e.candidate();
        st.commit = Some(
            e.record
                .as_ref()
                .map(|r| r.commit.clone())
                .unwrap_or_else(|| e.release.target_commitish.clone()),
        );
        st.manifest_version = e.record.as_ref().map(|r| r.manifest_version.clone());
        st.evidence_links.insert(0, e.release.url.clone());
        if let Some(rec) = &e.record {
            st.failing_gates = rec.failing.clone();
        }
    };
    if let Some(p) = f.existing.iter().find(|e| !e.release.draft) {
        take(&mut st, p);
        st.state = ReleaseState::Promoted;
        if let (Some(r), Some(rec), Some(tag)) = (&f.receipt, &p.record, &f.tag) {
            if receipt_matches(r, &p.release, rec, tag) {
                st.state = ReleaseState::Verified;
            }
        }
        if st.state == ReleaseState::Promoted {
            st.failing_gates.push(FailingGate {
                gate_id: "verify".into(),
                outcome: "blocked".into(),
                reason: "published; no authenticated verification receipt yet".into(),
            });
        }
        return st;
    }
    if let Some(d) = f
        .existing
        .iter()
        .filter(|e| e.release.draft)
        .max_by_key(|e| e.candidate().unwrap_or(0))
    {
        take(&mut st, d);
        st.state = match &d.record {
            Some(r)
                if r.artifacts
                    .iter()
                    .all(|a| d.release.asset(&a.name).is_some()) =>
            {
                match r.state {
                    ReleaseState::Validated => ReleaseState::Validated,
                    ReleaseState::CandidateBuilt => ReleaseState::CandidateBuilt,
                    ReleaseState::Promoted => ReleaseState::Validated,
                    _ => ReleaseState::Prepared,
                }
            }
            _ => ReleaseState::Prepared,
        };
        return st;
    }
    if let Some(r) = newest_run {
        st.commit = run_commit(&r.title, &f.version);
        if r.status != "completed" {
            st.state = ReleaseState::Prepared;
        } else if let Some(n) = f.recorded.iter().max() {
            st.state = ReleaseState::Abandoned;
            st.candidate = Some(*n);
        } else {
            st.state = ReleaseState::Prepared;
            if r.conclusion.as_deref() != Some("success") {
                st.failing_gates.push(FailingGate {
                    gate_id: "candidate-run".into(),
                    outcome: match r.conclusion.as_deref() {
                        Some("cancelled") | Some("timed_out") => "blocked",
                        _ => "failed",
                    }
                    .into(),
                    reason: format!(
                        "run {} concluded {} before a draft was assembled",
                        r.id,
                        r.conclusion.clone().unwrap_or_default()
                    ),
                });
            }
        }
    }
    st
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub mod tests_support;
