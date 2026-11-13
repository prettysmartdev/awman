//! `cargo xtask release verify public`: the post-publish receipt.
//!
//! Owner: workflow step `candidate-promote-pipeline` (see the WI 0144
//! ownership map). The signature of `run` is fixed by `release/mod.rs`.
//!
//! The native verify jobs of `promote.yml` download each asset from the
//! public URL, check it against the public `SHA256SUMS` and its attestation,
//! run its smoke and (on the release Mac) boot it, and leave one
//! `verified-<asset>.json` each. This step re-checks the public bytes on the
//! controller, requires every target's result, and writes the authenticated
//! receipt that `state::query` derives `verified` from. Published assets are
//! never modified.

use super::state::args::{outputs, read_json, write_json, Flags, Stop};
use super::state::decide;
use super::state::hub;
use super::state::ops;
use super::state::record::{self, Receipt};
use crate::evidence::digest;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask release verify public --version V --candidate N --results DIR --out DIR
       [--dry-run --namespace NS] [--repo owner/name] [--store DIR]
";

/// What one native verify job reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetResult {
    pub asset: String,
    pub sha256: String,
    pub sums_ok: bool,
    pub attestation_ok: bool,
    pub smoke_ok: bool,
    /// Signed Mac binary booted a guest offline; `null` for other targets.
    pub boot_ok: Option<bool>,
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    match args.split_first() {
        Some((s, rest)) if s == "public" => {
            let f = Flags::parse(rest, &["dry-run"])?;
            Ok(match public(&f)? {
                Ok(()) => ExitCode::SUCCESS,
                Err(stop) => stop.report(),
            })
        }
        Some((s, _)) if matches!(s.as_str(), "-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        _ => {
            eprint!("{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

/// Judge the native results against the record: every asset, exact bytes,
/// sums, attestation and smoke; the Mac asset must also have booted.
pub fn judge(rec: &record::ReleaseRecord, results: &[TargetResult]) -> Vec<String> {
    let mut out = Vec::new();
    for a in &rec.artifacts {
        let Some(r) = results.iter().find(|r| r.asset == a.name) else {
            out.push(format!("{}: no verification result", a.name));
            continue;
        };
        if r.sha256 != a.sha256 {
            out.push(format!(
                "{}: public bytes {} differ from the validated {}",
                a.name, r.sha256, a.sha256
            ));
        }
        if !r.sums_ok || !r.attestation_ok || !r.smoke_ok {
            out.push(format!("{}: sums/attestation/smoke failed", a.name));
        }
        if a.target.ends_with("apple-darwin") && r.boot_ok != Some(true) {
            out.push(format!(
                "{}: the published Mac binary did not boot a guest",
                a.name
            ));
        }
    }
    out
}

fn public(f: &Flags) -> Result<Result<(), Stop>> {
    let version = f.req("version")?;
    let candidate: u32 = f
        .req("candidate")?
        .parse()
        .context("--candidate is a number")?;
    let namespace = f.get("namespace").unwrap_or(record::PRODUCTION);
    let out = f.path("out")?;
    std::fs::create_dir_all(&out)?;
    let hub = hub::open(&hub::repo_from(f.get("repo"))?, f.get("store"))?;
    let e = match ops::find(hub.as_ref(), version, namespace, candidate)? {
        Ok(e) => e,
        Err(stop) => return Ok(Err(stop)),
    };
    let Some(rec) = e.record.clone() else {
        return Ok(Err(Stop::Failed(
            "the release has no release-state.json".into(),
        )));
    };
    let dry = f.on("dry-run");
    if !dry && e.release.draft {
        return Ok(Err(Stop::Failed(format!("{version} is not published yet"))));
    }
    let tag = hub.tag(version)?;
    if !dry && tag.as_deref() != Some(rec.commit.as_str()) {
        return Ok(Err(Stop::Failed(format!(
            "tag {version} is not at {}",
            rec.commit
        ))));
    }
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(f.path("results")?) {
        for p in entries.flatten().map(|e| e.path()) {
            collect(&p, &mut results);
        }
    }
    let mut problems = judge(&rec, &results);
    // Re-check the public bytes on the controller, not only in the native jobs.
    let dl = out.join("public");
    std::fs::create_dir_all(&dl)?;
    let fetch = |name: &str| -> Result<std::path::PathBuf> {
        let dest = dl.join(name);
        if dry {
            ops::download(hub.as_ref(), &e.release, name, &dest)?
                .with_context(|| format!("{name} missing from the draft"))?;
        } else {
            hub.public_download(version, name, &dest)?;
        }
        Ok(dest)
    };
    match fetch(record::SUMS) {
        Ok(p) => {
            if std::fs::read_to_string(&p)? != rec.sha256sums() {
                problems.push("public SHA256SUMS differs from the validated record".into());
            }
        }
        Err(err) => problems.push(format!("SHA256SUMS: {err:#}")),
    }
    for a in &rec.artifacts {
        match fetch(&a.name) {
            Ok(p) => {
                if digest::sha256_file(&p)? != a.sha256 {
                    problems.push(format!(
                        "{}: public bytes differ from the validated bytes",
                        a.name
                    ));
                }
                if !hub.verify_attestation(&p)? {
                    problems.push(format!("{}: attestation does not verify", a.name));
                }
            }
            Err(err) => problems.push(format!("{}: {err:#}", a.name)),
        }
    }
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("  {p}");
        }
        return Ok(Err(Stop::Failed(format!(
            "post-publish verification of {version} failed ({} problems); the release stays \
             published and unmodified. If the bytes are bad, publish a fixed patch version",
            problems.len()
        ))));
    }
    let receipt = Receipt {
        schema: 1,
        version: version.into(),
        candidate,
        release_id: e.release.id,
        tag_commit: tag.unwrap_or_default(),
        assets: rec
            .artifacts
            .iter()
            .map(|a| (a.name.clone(), a.sha256.clone()))
            .collect(),
        targets_verified: results.iter().map(|r| r.asset.clone()).collect(),
        run_id: super::state::args::run_env().run_id,
        verified_at: chrono::Utc::now(),
    };
    if !dry && !decide::receipt_matches(&receipt, &e.release, &rec, &receipt.tag_commit) {
        return Ok(Err(Stop::Failed(
            "receipt does not bind the publication".into(),
        )));
    }
    write_json(&out.join("receipt.json"), &receipt)?;
    outputs(&[("receipt_artifact", record::receipt_artifact(version))])?;
    println!(
        "verified {version}: {} assets from the public release",
        rec.artifacts.len()
    );
    Ok(Ok(()))
}

fn collect(p: &Path, out: &mut Vec<TargetResult>) {
    if p.is_dir() {
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                collect(&e.path(), out);
            }
        }
    } else if p
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("verified-"))
    {
        if let Ok(r) = read_json::<TargetResult>(p) {
            out.push(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_requires_every_target_and_the_mac_boot() {
        let mut rec = super::super::state::decide::tests_support::record();
        rec.artifacts.truncate(2);
        rec.artifacts[1].target = "aarch64-apple-darwin".into();
        let ok = |a: &record::ArtifactRecord, boot| TargetResult {
            asset: a.name.clone(),
            sha256: a.sha256.clone(),
            sums_ok: true,
            attestation_ok: true,
            smoke_ok: true,
            boot_ok: boot,
        };
        let all = vec![
            ok(&rec.artifacts[0], None),
            ok(&rec.artifacts[1], Some(true)),
        ];
        assert!(judge(&rec, &all).is_empty());
        assert_eq!(judge(&rec, &all[..1]).len(), 1, "missing target");
        let no_boot = vec![
            ok(&rec.artifacts[0], None),
            ok(&rec.artifacts[1], Some(false)),
        ];
        assert!(judge(&rec, &no_boot)[0].contains("did not boot"));
        let mut bad = all.clone();
        bad[0].sha256 = "0".repeat(64);
        assert!(judge(&rec, &bad)[0].contains("differ"));
    }
}
