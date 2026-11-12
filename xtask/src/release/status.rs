//! `cargo xtask release status`: read-only release state, failing gate and
//! evidence links (`make release-status VERSION=…`).
//!
//! Owner: workflow step `candidate-promote-pipeline` (see the WI 0144
//! ownership map). The signature of `run` is fixed by `release/mod.rs`.

use super::state::args::Flags;
use super::state::{self, hub, ReleaseState, ReleaseStatus};
use anyhow::{Context, Result};
use std::process::ExitCode;

/// `--repo`, else `$GITHUB_REPOSITORY`, else `gh repo view` of the checkout.
pub fn repo(flag: Option<&str>) -> Result<String> {
    if let Ok(r) = hub::repo_from(flag) {
        return Ok(r);
    }
    let out = std::process::Command::new("gh")
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ])
        .output()
        .context("running gh repo view")?;
    anyhow::ensure!(
        out.status.success(),
        "cannot determine the repository; pass --repo owner/name"
    );
    hub::repo_from(Some(String::from_utf8_lossy(&out.stdout).trim()))
}

pub fn render(s: &ReleaseStatus) -> String {
    let state = serde_json::to_value(s.state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut out = format!("{}: {state}\n", s.version);
    if let Some(n) = s.candidate {
        out.push_str(&format!("  candidate: {n}\n"));
    }
    if let Some(c) = &s.commit {
        out.push_str(&format!("  commit: {c}\n"));
    }
    if let Some(m) = &s.manifest_version {
        out.push_str(&format!("  gate manifest: {m}\n"));
    }
    if let Some(r) = s.run_id {
        out.push_str(&format!("  run: {r}\n"));
    }
    out.push_str(&format!(
        "  tag on origin: {}\n",
        if s.tag_on_origin { "yes" } else { "no" }
    ));
    for g in &s.failing_gates {
        out.push_str(&format!("  {} {}: {}\n", g.outcome, g.gate_id, g.reason));
    }
    for l in &s.evidence_links {
        out.push_str(&format!("  evidence: {l}\n"));
    }
    out.push_str(match s.state {
        ReleaseState::None => "  next: make release VERSION=… starts a candidate\n",
        ReleaseState::Prepared | ReleaseState::CandidateBuilt => {
            "  next: re-run make release VERSION=… to resume, or rerun the failed jobs\n"
        }
        ReleaseState::Validated => {
            "  next: promotion runs automatically; re-run make release to resume it\n"
        }
        ReleaseState::Promoted => {
            "  next: post-publish verification is pending or failed; re-run make release\n"
        }
        ReleaseState::Verified => "  published and verified\n",
        ReleaseState::Abandoned => "  next: make release VERSION=… starts a new candidate\n",
    });
    out
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    let Some((version, rest)) = args.split_first() else {
        eprintln!("usage: cargo xtask release status <vX.Y.Z> [--repo owner/name] [--json]");
        return Ok(ExitCode::from(2));
    };
    let f = Flags::parse(rest, &["json"])?;
    let repo = repo(f.get("repo"))?;
    let hub = hub::open(&repo, f.get("store"))?;
    let status = state::query_in(hub.as_ref(), version)?;
    if f.on("json") {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", render(&status));
    }
    Ok(ExitCode::SUCCESS)
}
