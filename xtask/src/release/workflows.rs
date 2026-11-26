//! Workflow checks the release command runs before dispatching anything:
//!
//! * permission closure (incident I7): every local `uses: ./…` call grants
//!   at least the scopes each callee job requests, otherwise GitHub starts
//!   the run as `startup_failure` with zero jobs;
//! * the `release.yml` contract (design 5.4(e), 7.7): dispatch inputs,
//!   run-name key, produce/validate roles, hosted runners only, no
//!   environment, secrets or local calls.

use super::error::{Step, Stop};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::Path;

fn level(v: &str) -> u8 {
    match v {
        "write" => 2,
        "read" => 1,
        _ => 0,
    }
}

const SCOPES: &[&str] = &[
    "actions",
    "attestations",
    "checks",
    "contents",
    "deployments",
    "discussions",
    "id-token",
    "issues",
    "models",
    "packages",
    "pages",
    "pull-requests",
    "repository-projects",
    "security-events",
    "statuses",
];

/// `None` = no permissions block (inherits the repository default).
fn grant(v: &Value) -> Option<BTreeMap<String, u8>> {
    match v {
        Value::Null => None,
        Value::String(s) if s == "read-all" => {
            Some(SCOPES.iter().map(|s| (s.to_string(), 1)).collect())
        }
        Value::String(s) if s == "write-all" => {
            Some(SCOPES.iter().map(|s| (s.to_string(), 2)).collect())
        }
        Value::Mapping(m) => Some(
            m.iter()
                .filter_map(|(k, v)| {
                    Some((k.as_str()?.to_string(), level(v.as_str().unwrap_or("none"))))
                })
                .collect(),
        ),
        _ => Some(BTreeMap::new()),
    }
}

/// Every closure violation, as `file job → callee job needs scope:level`.
pub fn closure_errors(dir: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return errors;
    };
    let mut docs = BTreeMap::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "yml" || x == "yaml") {
            match std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| serde_yaml::from_str::<Value>(&t).ok())
            {
                Some(doc) => {
                    docs.insert(p.file_name().unwrap().to_string_lossy().to_string(), doc);
                }
                None => errors.push(format!("{} is not valid YAML", p.display())),
            }
        }
    }
    for (file, doc) in &docs {
        let top = grant(&doc["permissions"]);
        let Some(jobs) = doc["jobs"].as_mapping() else {
            continue;
        };
        for (job_name, job) in jobs {
            let Some(uses) = job["uses"].as_str() else {
                continue;
            };
            let Some(callee) = uses.strip_prefix("./.github/workflows/") else {
                continue;
            };
            let callee = callee.split('@').next().unwrap_or(callee);
            let Some(callee_doc) = docs.get(callee) else {
                errors.push(format!(
                    "{file} job {} calls missing {callee}",
                    job_name.as_str().unwrap_or("?")
                ));
                continue;
            };
            let caller = grant(&job["permissions"]).or_else(|| top.clone());
            let Some(caller) = caller else { continue };
            let callee_top = grant(&callee_doc["permissions"]);
            for (cj_name, cj) in callee_doc["jobs"].as_mapping().cloned().unwrap_or_default() {
                let need = grant(&cj["permissions"])
                    .or_else(|| callee_top.clone())
                    .unwrap_or_default();
                for (scope, lvl) in need {
                    if caller.get(&scope).copied().unwrap_or(0) < lvl {
                        errors.push(format!(
                            "{file} job {} → {callee} job {} needs {scope}: {} but is granted {}",
                            job_name.as_str().unwrap_or("?"),
                            cj_name.as_str().unwrap_or("?"),
                            ["none", "read", "write"][lvl as usize],
                            ["none", "read", "write"]
                                [caller.get(&scope).copied().unwrap_or(0) as usize]
                        ));
                    }
                }
            }
        }
    }
    errors
}

pub fn closure(dir: &Path) -> Step {
    match closure_errors(dir).into_iter().next() {
        Some(e) => Err(Stop::new("workflow-closure", e)),
        None => Ok(()),
    }
}

/// The `release.yml` contract. Returns every violation.
pub fn release_contract(text: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let doc: Value = match serde_yaml::from_str(text) {
        Ok(d) => d,
        Err(e) => return vec![format!("release.yml is not valid YAML: {e}")],
    };
    let on = &doc["on"];
    let inputs = &on["workflow_dispatch"]["inputs"];
    for input in ["asset", "version", "commit", "key"] {
        if inputs[input].is_null() {
            errors.push(format!("workflow_dispatch input `{input}` is missing"));
        }
    }
    if on.as_mapping().is_some_and(|m| m.len() != 1) {
        errors.push("release.yml must only be started by workflow_dispatch".into());
    }
    let run_name = doc["run-name"].as_str().unwrap_or("");
    for part in ["inputs.version", "inputs.asset", "inputs.key"] {
        if !run_name.contains(part) {
            errors.push(format!("run-name must contain {part}"));
        }
    }
    if grant(&doc["permissions"]) != Some(BTreeMap::from([("contents".to_string(), 1)])) {
        errors.push("top-level permissions must be exactly `contents: read`".into());
    }
    if text.contains("secrets.") {
        errors.push("release.yml must not use secrets".into());
    }
    let jobs = doc["jobs"].as_mapping().cloned().unwrap_or_default();
    let names: Vec<String> = jobs
        .keys()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    if names != ["produce", "validate"] {
        errors.push(format!(
            "jobs must be exactly [produce, validate], found {names:?}"
        ));
    }
    for (name, job) in &jobs {
        let name = name.as_str().unwrap_or("?");
        if !job["environment"].is_null() {
            errors.push(format!("job {name} must not use an environment"));
        }
        if job["uses"].as_str().is_some() {
            errors.push(format!("job {name} must not call a reusable workflow"));
        }
        let runs_on = serde_yaml::to_string(&job["runs-on"]).unwrap_or_default();
        if runs_on.contains("self-hosted") {
            errors.push(format!("job {name} must run on hosted runners"));
        }
        if let Some(steps) = job["steps"].as_sequence() {
            for s in steps {
                if s["uses"].as_str().is_some_and(|u| u.starts_with("./")) {
                    errors.push(format!("job {name} uses a local action"));
                }
                if let Some(u) = s["uses"].as_str() {
                    let pinned = u.split_once('@').is_some_and(|(_, r)| {
                        r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit())
                    });
                    if !pinned {
                        errors.push(format!(
                            "job {name} action {u} is not pinned to a commit SHA"
                        ));
                    }
                }
            }
        }
    }
    let produce = &doc["jobs"]["produce"]["permissions"];
    if level(produce["id-token"].as_str().unwrap_or("")) != 2
        || level(produce["attestations"].as_str().unwrap_or("")) != 2
    {
        errors.push("produce must grant id-token: write and attestations: write".into());
    }
    let needs = serde_yaml::to_string(&doc["jobs"]["validate"]["needs"]).unwrap_or_default();
    if !needs.contains("produce") {
        errors.push("validate must need produce".into());
    }
    errors
}

/// `cargo xtask release check-workflows [--root DIR]`.
pub fn run(args: &[String]) -> anyhow::Result<std::process::ExitCode> {
    let root = match args {
        [flag, dir] if flag == "--root" => std::path::PathBuf::from(dir),
        [] => std::env::current_dir()?,
        _ => anyhow::bail!("usage: cargo xtask release check-workflows [--root DIR]"),
    };
    let dir = root.join(".github/workflows");
    let mut errors = closure_errors(&dir);
    match std::fs::read_to_string(dir.join("release.yml")) {
        Ok(text) => errors.extend(release_contract(&text)),
        Err(_) => errors.push("release.yml is missing".into()),
    }
    for e in &errors {
        eprintln!("check-workflows: {e}");
    }
    Ok(if errors.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_call_without_the_callee_scope_is_a_closure_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("nightly.yml"),
            "on: {schedule: [{cron: '0 0 * * *'}]}\npermissions: {contents: read}\njobs:\n  ci:\n    uses: ./.github/workflows/ci.yml\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("ci.yml"),
            "on: {workflow_call: {}}\njobs:\n  plan:\n    runs-on: ubuntu-24.04\n    permissions: {contents: read, pull-requests: read}\n    steps: []\n",
        )
        .unwrap();
        let errors = closure_errors(tmp.path());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("pull-requests: read but is granted none"));
        assert_eq!(closure(tmp.path()).unwrap_err().code, "workflow-closure");
        std::fs::write(
            tmp.path().join("nightly.yml"),
            "on: {schedule: [{cron: '0 0 * * *'}]}\npermissions: {contents: read, pull-requests: read}\njobs:\n  ci:\n    uses: ./.github/workflows/ci.yml\n",
        )
        .unwrap();
        assert!(closure_errors(tmp.path()).is_empty());
    }

    #[test]
    fn the_repository_release_workflow_meets_its_contract() {
        let root = crate::gates::root();
        let text = std::fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
        assert_eq!(release_contract(&text), Vec::<String>::new());
        let bad = text.replace("'ubuntu-22.04' }}", "'self-hosted' }}");
        assert!(!release_contract(&bad).is_empty());
    }
}
