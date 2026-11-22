//! `ci.yml` and `nightly.yml` are rendered here from the manifest (`outputs`);
//! `candidate.yml`, `promote.yml`, `fixtures.yml` are declared in
//! their own producers and are only verified. The verifier is deliberately
//! conservative and is supplemented by pinned actionlint in the `lint` job.
use crate::manifest::Manifest;

mod ci;
mod graph;
mod nightly;
mod pieces;
mod policy;

#[allow(unused_imports)]
pub use graph::{build_job_id, ci_builds, ci_test_jobs, lint_suites, test_job_id, TestJob};

/// Workflow files this crate renders (path relative to the repository root).
pub fn outputs(m: &Manifest) -> anyhow::Result<Vec<(String, String)>> {
    Ok(vec![
        (".github/workflows/ci.yml".into(), ci::render(m)?),
        (".github/workflows/nightly.yml".into(), nightly::render(m)?),
    ])
}
use std::{collections::BTreeSet, path::Path};
pub fn verify_text(path: &str, text: &str, expected: &crate::manifest::Workflow) -> Vec<String> {
    let mut errors = policy::verify(path, text);
    let mut jobs = BTreeSet::new();
    let mut active: Option<(String, bool)> = None;
    let mut in_jobs = false;
    for line in text.lines() {
        if line == "jobs:" {
            in_jobs = true;
            continue;
        }
        if in_jobs && !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            in_jobs = false;
        }
        if in_jobs
            && line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
        {
            if let Some((id, false)) = active.take() {
                errors.push(format!("{path}: job {id} missing timeout-minutes"));
            }
            let id = line.trim().trim_end_matches(':').to_owned();
            jobs.insert(id.clone());
            active = Some((id, false));
        }
        if line.starts_with("    timeout-minutes:") {
            if let Some((id, has)) = &mut active {
                *has = true;
                let value = line
                    .trim()
                    .strip_prefix("timeout-minutes:")
                    .unwrap()
                    .trim()
                    .parse::<u64>();
                if !value.is_ok_and(|n| n > 0 && n <= expected.timeout_minutes) {
                    errors.push(format!(
                        "{path}: job {id} timeout exceeds manifest budget or is not literal"
                    ));
                }
            }
        }
        if line.starts_with("    uses:") {
            // A job that calls a reusable workflow cannot carry timeout-minutes (GitHub
            // rejects it); the called workflow's own jobs are linted instead.
            if let Some((_, has)) = &mut active {
                *has = true;
            }
        }
        if let Some((_, action)) = line.split_once("uses:") {
            let action = action.trim();
            if !action.starts_with("./") {
                let pin = action
                    .split_once('@')
                    .map(|(_, p)| p.split_whitespace().next().unwrap_or(""));
                if !pin.is_some_and(|p| p.len() == 40 && p.bytes().all(|b| b.is_ascii_hexdigit())) {
                    errors.push(format!(
                        "{path}: action/reusable workflow not pinned to full SHA: {action}"
                    ));
                }
            }
        }
    }
    if let Some((id, false)) = active {
        errors.push(format!("{path}: job {id} missing timeout-minutes"));
    }
    for id in &expected.jobs {
        // A manifest job id is satisfied by that job or by its rendered per-build
        // and per-tier variants (`build-<build>`, `test-<tier>-<platform>`).
        if !jobs.iter().any(|j| {
            j == id || (matches!(id.as_str(), "build" | "test") && j.starts_with(&format!("{id}-")))
        }) {
            errors.push(format!("{path}: missing or renamed required job {id}"));
        }
    }
    for command in &expected.commands {
        if !text
            .lines()
            .any(|line| !line.trim_start().starts_with('#') && line.contains(command))
        {
            errors.push(format!("{path}: missing manifest command {command}"));
        }
    }
    errors
}
pub fn verify(root: &Path, m: &Manifest) -> Vec<String> {
    let mut errors = Vec::new();
    for (path, expected) in &m.workflows {
        match std::fs::read_to_string(root.join(path)) {
            Ok(t) => errors.extend(verify_text(path, &t, expected)),
            Err(_) => errors.push(format!("missing required workflow {path}")),
        }
    }
    if let Ok(entries) = std::fs::read_dir(root.join(".github/workflows")) {
        for entry in entries.flatten() {
            let path = format!(".github/workflows/{}", entry.file_name().to_string_lossy());
            if matches!(
                entry.path().extension().and_then(|x| x.to_str()),
                Some("yml" | "yaml")
            ) && !m.workflows.contains_key(&path)
            {
                errors.push(format!("unregistered workflow {path}"));
            }
        }
    }
    errors
}
