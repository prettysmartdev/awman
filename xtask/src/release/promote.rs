//! `cargo xtask release promote`: re-verify the candidate, create the final
//! annotated tag, publish the draft (`.github/workflows/promote.yml`).
//!
//! Owner: workflow step `candidate-promote-pipeline` (see the WI 0144
//! ownership map). The signature of `run` is fixed by `release/mod.rs`.
//!
//! Promotion compiles, tests and signs nothing. Every step re-downloads the
//! bytes and re-checks every gate, policy and expiry immediately before its
//! one irreversible call, so the steps can run (and be resumed) separately.

use super::state::args::{outputs, write_json, Flags, Stop};
use super::state::decide::{self, Downloaded, PublishAction, TagAction};
use super::state::hub::{self, Hub, ReleaseEdit, ReleaseFact};
use super::state::inputs::Policy;
use super::state::ops;
use super::state::record::{self, ReleaseRecord};
use super::state::ReleaseState;
use crate::evidence::{digest, schema::GateEvidence};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask release promote <step> --version V --candidate N
       [--commit SHA] [--namespace NS] [--dry-run] --main-checkout DIR --out DIR
       [--repo owner/name] [--store DIR]

  verify-inputs   re-download every asset and check it against the evidence
  tag             create the annotated vX.Y.Z tag at the candidate commit
  publish         publish the draft (after the tag exists)
";

pub fn run(args: &[String]) -> Result<ExitCode> {
    let Some((step, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let f = Flags::parse(rest, &["dry-run"])?;
    let result = match step.as_str() {
        "verify-inputs" => verify_step(&f),
        "tag" => tag(&f),
        "publish" => publish(&f),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        other => {
            eprintln!("xtask release promote: unknown step `{other}`\n\n{USAGE}");
            return Ok(ExitCode::from(2));
        }
    }?;
    Ok(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(stop) => stop.report(),
    })
}

/// A candidate that passed every promotion check just now.
pub struct Verified {
    pub release: ReleaseFact,
    pub record: ReleaseRecord,
    pub tag_commit: Option<String>,
    pub main_tip: String,
    pub policy_checkout: PathBuf,
}

fn sha_of(path: &Path) -> Option<String> {
    digest::sha256_file(path).ok()
}

/// Download, hash and check everything. Shared by all three steps.
pub fn verify(hub: &dyn Hub, f: &Flags, out: &Path) -> Result<Result<Verified, Stop>> {
    let version = f.req("version")?;
    let candidate: u32 = f
        .req("candidate")?
        .parse()
        .context("--candidate is a number")?;
    let dry_run = f.on("dry-run");
    let namespace = f.get("namespace").unwrap_or(record::PRODUCTION);
    if dry_run == (namespace == record::PRODUCTION) {
        return Ok(Err(Stop::Failed(
            "a dry run promotes only its own rehearsal namespace; a real run only production"
                .into(),
        )));
    }
    let main_checkout = PathBuf::from(f.get("main-checkout").unwrap_or("."));
    let e = match ops::find(hub, version, namespace, candidate)? {
        Ok(e) => e,
        Err(stop) => return Ok(Err(stop)),
    };
    let Some(rec) = e.record.clone() else {
        return Ok(Err(Stop::Failed(format!(
            "candidate {candidate} has no release-state.json"
        ))));
    };
    if let Some(c) = f.get("commit") {
        if c != rec.commit {
            return Ok(Err(Stop::Failed(format!(
                "candidate {candidate} was built from {}, not {c}",
                rec.commit
            ))));
        }
    }
    let release = e.release;
    let dl = out.join("download");
    let _ = std::fs::remove_dir_all(&dl);
    std::fs::create_dir_all(&dl)?;
    let mut downloaded = Vec::new();
    for a in &rec.artifacts {
        let path = dl.join(&a.name);
        let Some(sha) = ops::download(hub, &release, &a.name, &path)? else {
            continue;
        };
        downloaded.push(Downloaded {
            name: a.name.clone(),
            sha256: sha,
            attestation_verified: hub.verify_attestation(&path)?,
        });
    }
    let sums_path = dl.join(record::SUMS);
    ops::download(hub, &release, record::SUMS, &sums_path)?;
    let sums = std::fs::read_to_string(&sums_path).unwrap_or_default();
    let gate_path = dl.join(record::GATE_ASSET);
    ops::download(hub, &release, record::GATE_ASSET, &gate_path)?;
    let gate_sha = sha_of(&gate_path);
    let gate: Option<GateEvidence> = super::state::args::read_json(&gate_path).ok();
    let main_path = dl.join(record::MAIN_GATE_ASSET);
    ops::download(hub, &release, record::MAIN_GATE_ASSET, &main_path)?;
    let main_sha = sha_of(&main_path);
    let (main_tip, policy_checkout) = hub.policy_checkout(&main_checkout, out, version)?;
    let m = crate::manifest::load(&policy_checkout)?;
    let now = chrono::Utc::now();
    let cur = decide::Current {
        now,
        main_contains_commit: hub.is_ancestor_of_main(&rec.commit)?,
        policy: Policy::current(&policy_checkout, version)?,
    };
    let mut findings = decide::evidence_findings(&rec, &m, &cur, !dry_run);
    findings.extend(decide::promotion_findings(
        &m,
        &rec,
        &release,
        &downloaded,
        &sums,
        gate.as_ref().zip(gate_sha.as_deref()),
        main_sha.as_deref(),
        now,
    ));
    if findings.is_empty() {
        let validation_path = dl.join(record::VALIDATION_ASSET);
        ops::download(hub, &release, record::VALIDATION_ASSET, &validation_path)?;
        if !validation_path.is_file() || !hub.verify_attestation(&validation_path)? {
            return Ok(Err(Stop::Failed(
                "candidate has no authenticated validation statement".into(),
            )));
        }
        let validation: record::Validation = super::state::args::read_json(&validation_path)?;
        if validation.record.identity() != rec.identity()
            || validation.record.candidate != rec.candidate
            || validation.record.release_id != Some(release.id)
            || validation.record.runs.validation != rec.runs.validation
            || validation.record.expires_at != rec.expires_at
            || validation.record.gates != rec.gates
            || validation.record.inputs != rec.inputs
            || validation.record.compute_candidate_digest()? != rec.compute_candidate_digest()?
            || Some(validation.gate_sha256.as_str()) != gate_sha.as_deref()
            || validation.record.state != ReleaseState::Validated
        {
            return Ok(Err(Stop::Failed(
                "validation statement does not bind this candidate/gate/run".into(),
            )));
        }
    }
    let tag_commit = hub.tag(version)?;
    write_json(
        &out.join("verify-inputs.json"),
        &serde_json::json!({"version": version, "candidate": candidate, "commit": rec.commit,
            "release_id": release.id, "findings": findings.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "checked_at": now}),
    )?;
    if !findings.is_empty() {
        for s in &findings[1..] {
            eprintln!("  {s}");
        }
        return Ok(Err(findings.remove(0)));
    }
    Ok(Ok(Verified {
        release,
        record: rec,
        tag_commit,
        main_tip,
        policy_checkout,
    }))
}

fn open(f: &Flags) -> Result<Box<dyn Hub>> {
    hub::open(&hub::repo_from(f.get("repo"))?, f.get("store"))
}

fn verify_step(f: &Flags) -> Result<Result<(), Stop>> {
    let out = f.path("out")?;
    let hub = open(f)?;
    let v = match verify(hub.as_ref(), f, &out)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    let matrix: Vec<_> = v
        .record
        .inputs
        .targets
        .iter()
        .map(|t| {
            let sha = v
                .record
                .artifact(&t.asset)
                .map(|a| a.sha256.clone())
                .unwrap_or_default();
            serde_json::json!({"asset": t.asset, "target": t.triple, "build": t.build,
                "runs_on": t.runs_on, "sha256": sha,
                "self_hosted": t.runs_on.iter().any(|l| l == "self-hosted")})
        })
        .collect();
    let m = crate::manifest::load(Path::new(f.get("main-checkout").unwrap_or(".")))?;
    let nextest = m.tools.get("nextest");
    let nextest_sha = nextest
        .and_then(|t| t.sha256.get("universal-apple-darwin"))
        .cloned()
        .unwrap_or_default();
    let nextest_url = nextest
        .map(|t| {
            format!(
                "{}/cargo-nextest-{}-universal-apple-darwin.tar.gz",
                t.source, t.version
            )
        })
        .unwrap_or_default();
    outputs(&[
        ("ok", "true".into()),
        ("release_id", v.release.id.to_string()),
        ("commit", v.record.commit.clone()),
        ("published", (!v.release.draft).to_string()),
        // The pinned runner for the prebuilt Mac boot harness (no Cargo needed).
        ("nextest_url", nextest_url),
        ("nextest_sha256", nextest_sha),
        ("source_run", v.record.runs.candidate.to_string()),
        (
            "validation_run",
            v.record
                .runs
                .validation
                .unwrap_or(v.record.runs.candidate)
                .to_string(),
        ),
        ("targets", serde_json::to_string(&matrix)?),
    ])?;
    println!(
        "candidate {} of {} verified for promotion ({} assets, gates {:?})",
        v.record.candidate,
        v.record.version,
        v.record.artifacts.len(),
        decide::required_gates(&crate::manifest::load(Path::new(
            f.get("main-checkout").unwrap_or(".")
        ))?)
    );
    Ok(Ok(()))
}

fn tag(f: &Flags) -> Result<Result<(), Stop>> {
    if f.on("dry-run") {
        println!("dry run: no tag is created");
        return Ok(Ok(()));
    }
    let out = f.path("out")?;
    let hub = open(f)?;
    // Recheck inside the publication lock, immediately before the tag.
    let v = match verify(hub.as_ref(), f, &out)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    let version = &v.record.version;
    match decide::tag_action(v.tag_commit.as_deref(), &v.record.commit, version) {
        Err(stop) => Ok(Err(stop)),
        Ok(TagAction::Present) => {
            println!("Tag {version} already on origin at {}", v.record.commit);
            Ok(Ok(()))
        }
        Ok(TagAction::Create) => {
            if hub.main_head()? != v.main_tip {
                return Ok(Err(Stop::Failed(
                    "protected main moved before tagging; retry verification".into(),
                )));
            }
            let message = format!("awman {version} (candidate {})", v.record.candidate);
            if let Err(e) = hub.create_tag(version, &v.record.commit, &message) {
                // A concurrent creation of the same tag at the same commit is fine.
                if hub.tag(version)?.as_deref() == Some(v.record.commit.as_str()) {
                    println!("Tag {version} already on origin at {}", v.record.commit);
                    return Ok(Ok(()));
                }
                return Ok(Err(Stop::Blocked(format!(
                    "creating tag {version} failed: {e:#}"
                ))));
            }
            println!("Created annotated tag {version} at {}", v.record.commit);
            Ok(Ok(()))
        }
    }
}

fn publish(f: &Flags) -> Result<Result<(), Stop>> {
    if f.on("dry-run") {
        println!("dry run: nothing is published");
        return Ok(Ok(()));
    }
    let out = f.path("out")?;
    let hub = open(f)?;
    let v = match verify(hub.as_ref(), f, &out)? {
        Ok(v) => v,
        Err(stop) => return Ok(Err(stop)),
    };
    let version = v.record.version.clone();
    match decide::publish_action(&v.release, v.tag_commit.as_deref(), &v.record) {
        Err(stop) => Ok(Err(stop)),
        Ok(PublishAction::AlreadyPublished) => {
            println!("Release {version} is already published");
            outputs(&[("release_id", v.release.id.to_string())])?;
            Ok(Ok(()))
        }
        Ok(PublishAction::Publish) => {
            if hub.main_head()? != v.main_tip {
                return Ok(Err(Stop::Failed(
                    "protected main moved before publication; retry verification".into(),
                )));
            }
            let notes = std::fs::read_to_string(
                v.policy_checkout
                    .join(format!("docs/releases/{version}.md")),
            )
            .context("reading the release notes")?;
            // Freeze the journal before publication: assets of a published
            // release are never changed again.
            let mut rec = v.record.clone();
            rec.state = ReleaseState::Promoted;
            rec.runs.promote = Some(super::state::args::run_env().run_id);
            if rec != v.record {
                if let Err(stop) = ops::put_state(hub.as_ref(), v.release.id, &rec, &out)? {
                    return Ok(Err(stop));
                }
            }
            if hub.main_head()? != v.main_tip {
                return Ok(Err(Stop::Failed(
                    "protected main moved before publication; retry verification".into(),
                )));
            }
            hub.edit_release(
                v.release.id,
                &ReleaseEdit {
                    draft: Some(false),
                    name: Some(version.clone()),
                    body: Some(notes),
                },
            )?;
            println!(
                "Published {version} (candidate {}) from draft {}",
                rec.candidate, v.release.id
            );
            outputs(&[("release_id", v.release.id.to_string())])?;
            Ok(Ok(()))
        }
    }
}
