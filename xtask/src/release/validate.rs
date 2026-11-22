//! `cargo xtask release validate`: candidate validation of the current main
//! commit without releasing (`make apple-prerelease` or the narrower
//! `make apple-silicon-prerelease`). Dispatches a
//! rehearsal candidate (dry run: no tag, nothing published, draft deleted at
//! the end) and watches it.
//!
//! Owner: workflow step `candidate-promote-pipeline` (see the WI 0144
//! ownership map). The signature of `run` is fixed by `release/mod.rs`.

use super::state::args::Flags;
use super::state::hub::{self, RunFact};
use anyhow::{Context, Result};
use std::process::{Command, ExitCode};

fn newest_run(runs: Vec<RunFact>, title: &str, after: u64) -> Option<RunFact> {
    runs.into_iter()
        .filter(|r| r.title == title && r.id > after)
        .max_by_key(|r| r.id)
}

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
    let title = format!("Release {version} ({commit}) [rehearsal]");
    let previous_run = newest_run(hub.runs("candidate.yml")?, &title, 0)
        .map(|r| r.id)
        .unwrap_or(0);
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
        .args(["-f", "dry_run=true", "-f", "promote=true"])
        .args([
            "-f",
            r#"operator_record={"source":"cargo xtask release validate"}"#,
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
    for _ in 0..60 {
        let run = newest_run(hub.runs("candidate.yml")?, &title, previous_run);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: u64, title: &str) -> RunFact {
        RunFact {
            id,
            title: title.into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            created_at: "2026-11-21T00:00:00Z".into(),
            head_branch: "main".into(),
            event: "workflow_dispatch".into(),
            url: format!("https://example.invalid/runs/{id}"),
        }
    }

    #[test]
    fn rehearsal_watch_selects_newly_completed_run() {
        let title = "Release v0.14.0 (commit) [rehearsal]";
        let runs = vec![
            run(100, title),
            run(101, "another release"),
            run(102, title),
        ];
        assert_eq!(newest_run(runs.clone(), title, 100).unwrap().id, 102);
        assert!(newest_run(runs, title, 102).is_none());
    }
}
