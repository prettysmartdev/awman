//! Check executable references outside the generated contract fragment too.
use crate::manifest::Manifest;
use anyhow::Result;
use std::{collections::BTreeSet, path::Path};

pub fn references(
    text: &str,
    targets: &BTreeSet<String>,
    jobs: &BTreeSet<String>,
    suites: &BTreeSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let spans: Vec<_> = text.match_indices('`').map(|(i, _)| i).collect();
    for pair in spans.chunks_exact(2) {
        let value = &text[pair[0] + 1..pair[1]];
        if value.contains('\n') || value.is_empty() {
            continue;
        }
        if let Some(args) = value.strip_prefix("make ") {
            for target in args
                .split_whitespace()
                .take_while(|v| !v.starts_with('#') && !v.contains('='))
            {
                if target.starts_with('-')
                    || target.contains('=')
                    || target.contains('$')
                    || target.contains('<')
                    || target.contains('|')
                    || target == "…"
                {
                    continue;
                }
                if !targets.contains(target) {
                    out.push(format!("unknown Make target {target} in `{value}`"));
                }
            }
        }
        let before = &text[..pair[0]];
        let is_job = before.ends_with("job ") || before.ends_with("jobs ");
        let is_suite = before.ends_with("suite ") || before.ends_with("suites ");
        for (label, ids, relevant) in [("job", jobs, is_job), ("suite", suites, is_suite)] {
            if !relevant {
                continue;
            }
            for id in value.split([',', ' ']).filter(|s| !s.is_empty()) {
                let matches = ids.contains(id)
                    || id.split_once('*').is_some_and(|(prefix, suffix)| {
                        ids.iter()
                            .any(|i| i.starts_with(prefix) && i.ends_with(suffix))
                    });
                if !matches && !id.contains('<') && !id.contains('…') {
                    out.push(format!("unknown {label} {id} in `{value}`"));
                }
            }
        }
    }
    out
}

pub fn check(root: &Path, manifest: &Manifest) -> Result<Vec<String>> {
    let makefile = std::fs::read_to_string(root.join("Makefile"))?;
    let targets: BTreeSet<_> = makefile
        .lines()
        .filter(|l| !l.starts_with(['#', '\t', ' ', '.']))
        .filter_map(|l| l.split_once(':'))
        .filter(|(_, tail)| !tail.starts_with('='))
        .flat_map(|(head, _)| head.split_whitespace().map(String::from))
        .collect();
    let mut jobs = BTreeSet::new();
    for file in std::fs::read_dir(root.join(".github/workflows"))? {
        let path = file?.path();
        if path.extension().is_none_or(|e| e != "yml" && e != "yaml") {
            continue;
        }
        let doc: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(path)?)?;
        if let Some(entries) = doc["jobs"].as_mapping() {
            for (id, job) in entries {
                if let Some(id) = id.as_str() {
                    jobs.insert(id.into());
                }
                if let Some(name) = job["name"].as_str() {
                    jobs.insert(name.into());
                }
            }
        }
    }
    let suites = manifest.suites.keys().cloned().collect();
    fn walk(path: &Path, paths: &mut Vec<std::path::PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(path)? {
            let path = entry?.path();
            if path.is_dir() && path.file_name().is_some_and(|n| n == "releases") {
                // Versioned release notes describe the commands shipped then.
                continue;
            }
            if path.is_dir() {
                walk(&path, paths)?;
            } else if path.extension().is_some_and(|e| e == "md") {
                paths.push(path);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    for directory in ["aspec/devops", "docs"] {
        walk(&root.join(directory), &mut paths)?;
    }
    let mut out = Vec::new();
    for path in paths {
        for finding in references(&std::fs::read_to_string(&path)?, &targets, &jobs, &suites) {
            out.push(format!("{}: {finding}", path.strip_prefix(root)?.display()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_references_outside_generated_block_are_rejected() {
        let targets = BTreeSet::from(["test".into()]);
        let jobs = BTreeSet::from(["gate".into()]);
        let suites = BTreeSet::from(["component-data".into()]);
        let errors = references(
            "Run `make test-fast` (job `fast-nonexistent`), suite `component-missing`.",
            &targets,
            &jobs,
            &suites,
        );
        assert_eq!(errors.len(), 3);
        assert!(references(
            "Run `make test` (job `gate`), suite `component-data`.",
            &targets,
            &jobs,
            &suites
        )
        .is_empty());
    }
}
