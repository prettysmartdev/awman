//! `cargo xtask release validate`: candidate validation of the current main
//! commit without releasing (`make apple-silicon-prerelease`). Dispatches a
//! rehearsal candidate (dry run: no tag, nothing published, draft deleted at
//! the end) and watches it.
//!
//! Owner: workflow step `candidate-promote-pipeline` (see the WI 0144
//! ownership map). The signature of `run` is fixed by `release/mod.rs`.

use super::state::args::Flags;
use super::state::hub;
use anyhow::{Context, Result};
use std::process::{Command, ExitCode};

pub fn run(args: &[String]) -> Result<ExitCode> {
    let f = Flags::parse(args, &["no-watch"])?;
    let repo = super::status::repo(f.get("repo"))?;
    let hub = hub::open(&repo, f.get("store"))?;
    let version = format!(
        "v{}",
        super::candidate::cargo_version(std::path::Path::new("."))?
    );
    let commit = match f.get("commit") {
        Some(c) => c.to_owned(),
        None => hub.main_head()?,
    };
    let status = Command::new("gh")
        .args([
            "workflow",
            "run",
            "candidate.yml",
            "--repo",
            &repo,
            "--ref",
            "main",
        ])
        .args([
            "-f",
            &format!("version={version}"),
            "-f",
            &format!("commit={commit}"),
        ])
        .args(["-f", "dry_run=true", "-f", "promote=false"])
        .args([
            "-f",
            r#"operator_record={"source":"make apple-silicon-prerelease"}"#,
        ])
        .status()
        .context("running gh workflow run")?;
    if !status.success() {
        eprintln!("Could not start candidate validation for {version} at {commit}.");
        return Ok(ExitCode::from(1));
    }
    println!(
        "Started candidate validation of {version} at {commit} (rehearsal; nothing is published)"
    );
    if f.on("no-watch") {
        return Ok(ExitCode::SUCCESS);
    }
    let title = format!("Release {version} ({commit}) [rehearsal]");
    for _ in 0..60 {
        let run = hub
            .runs("candidate.yml")?
            .into_iter()
            .filter(|r| r.title == title && r.status != "completed")
            .max_by_key(|r| r.id);
        if let Some(r) = run {
            let watch = Command::new("gh")
                .args([
                    "run",
                    "watch",
                    &r.id.to_string(),
                    "--repo",
                    &repo,
                    "--exit-status",
                    "--interval",
                    "60",
                ])
                .status()?;
            return Ok(if watch.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    eprintln!("No candidate run appeared within 5 minutes. Check the repository's Actions tab.");
    Ok(ExitCode::from(1))
}
