use crate::manifest;
use anyhow::Result;
use std::{collections::BTreeSet, path::Path};
#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
}
impl Report {
    pub fn markdown(&self) -> String {
        let mut s=format!("# WI 0144 gate contract gaps\n\n{} gap(s). Report-only does not make these gates green.\n\n",self.errors.len());
        for e in &self.errors {
            s.push_str(&format!("- {e}\n"));
        }
        s
    }
}
pub fn check(root: &Path, allow_placeholders: bool) -> Result<Report> {
    let m = manifest::load(root)?;
    let mut r = Report::default();
    if let Err(e) = manifest::validate::validate(&m, chrono::Utc::now().date_naive()) {
        r.errors.push(format!("manifest: {e:#}"));
    }
    // `gates check` is the `pr` lint: it is red on every pin the pr gate depends
    // on. Pins that only main/nightly/candidate cells need (guest fixtures, the
    // previous release, hardware images) are enforced, unconditionally, by
    // `evidence aggregate` and `release candidate gate` for those gates.
    if !allow_placeholders {
        r.errors.extend(manifest::validate::pins_for_gate(&m, "pr"));
    }
    r.errors.extend(legacy_fallback_findings(root, &m));
    r.errors.extend(scenario_findings(root, &m));
    r.errors.extend(super::render::workflows::verify(root, &m));
    for (path, expected) in super::render::outputs(&m)? {
        if std::fs::read_to_string(root.join(&path)).ok().as_deref() != Some(&expected) {
            r.errors.push(format!(
                "{path}: generated content disagrees with manifest; run gates render"
            ));
        }
    }
    let make = std::fs::read_to_string(root.join("Makefile"))?;
    r.errors.extend(make_findings(&make));
    r.errors.extend(super::doc_refs::check(root, &m)?);
    let doc = super::render::docs::render(&m);
    for path in [
        "aspec/devops/cicd.md",
        "aspec/devops/localdev.md",
        "aspec/devops/testing.md",
    ] {
        let text = std::fs::read_to_string(root.join(path)).unwrap_or_default();
        if !text.contains(doc.trim_end()) {
            r.errors.push(format!(
                "{path}: missing or stale gates:begin contract documentation block"
            ));
        }
    }
    match super::discovery::metadata(root) {
        Ok(meta) => match super::discovery::audit(root, &m, &meta) {
            Ok(d) => r.errors.extend(d.findings),
            Err(e) => r.errors.push(format!("discovery incomplete: {e:#}")),
        },
        Err(e) => r
            .errors
            .push(format!("Cargo target discovery unavailable: {e:#}")),
    }
    // Compiled per-platform lists come from each build cell's archive job
    // (`test archive` writes expected-tests-<build>.json). The source walk above
    // is the static half; when a compiled list is present (the CI gate job
    // downloads them all) it must reconcile with the manifest. A missing list
    // never passes a gate: `evidence aggregate` refuses every cell without its
    // independent expected set.
    for (id, b) in &m.builds {
        if b.kind == "test" {
            let path = root.join(format!("target/gates-discovery/expected-tests-{id}.json"));
            if !path.exists() {
                continue;
            } else if let Err(e) = super::discovery::reconcile(root, &m, id, &std::fs::read(path)?)
            {
                r.errors.push(format!(
                    "build {id}: compiled inventory reconciliation failed: {e:#}"
                ));
            }
        }
    }
    r.errors.extend(switch_findings(root, &m)?);
    r.errors.sort();
    r.errors.dedup();
    Ok(r)
}
/// `[[legacy_fallback]]` rows name legacy files kept on purpose until a
/// cutover row (design 13.2) or a slice's shadow period closes. A row whose
/// path no longer exists is stale and must be deleted with the file; a row
/// without a recognised condition cannot be closed and is refused.
pub fn legacy_fallback_findings(root: &Path, m: &manifest::Manifest) -> Vec<String> {
    let mut errors = Vec::new();
    for row in &m.legacy_fallback {
        let known_condition = row
            .remove_when
            .strip_prefix('P')
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| (1..=11).contains(&n))
            || row
                .remove_when
                .strip_prefix("shadow:")
                .is_some_and(|s| !s.trim().is_empty());
        if !known_condition {
            errors.push(format!(
                "legacy_fallback {}: remove_when `{}` is neither a cutover row P1-P11 nor shadow:<owner>",
                row.path, row.remove_when
            ));
        }
        if !root.join(row.path.trim_end_matches('/')).exists() {
            errors.push(format!(
                "legacy_fallback {}: path no longer exists; delete the row in the same change",
                row.path
            ));
        }
    }
    errors
}

/// True when `path` (repository-relative) is inside a `[[legacy_fallback]]` row.
pub fn is_legacy_fallback(m: &manifest::Manifest, path: &str) -> bool {
    m.legacy_fallback.iter().any(|row| {
        if row.path.ends_with('/') {
            path.starts_with(&row.path)
        } else {
            path == row.path
        }
    })
}

pub fn make_findings(text: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for (target, commands) in make_contract() {
        let marker = format!("{target}:\n");
        let body = text
            .split_once(&marker)
            .map(|(_, tail)| {
                tail.lines()
                    .take_while(|l| l.starts_with('\t') || l.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        for command in commands {
            if !body
                .lines()
                .any(|l| l.starts_with('\t') && l.contains(command))
            {
                errors.push(format!("Makefile: {target} must invoke {command}"));
            }
        }
    }
    errors
}
pub fn make_contract() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "check",
            vec!["cargo xtask lint all", "cargo xtask test run --tier unit"],
        ),
        ("test", vec!["cargo xtask test run --gate local"]),
        ("pre-push", vec!["check test"]),
        ("test-unit", vec!["cargo xtask test run --tier unit"]),
        (
            "test-component",
            vec!["cargo xtask test run --tier component"],
        ),
        (
            "test-boundary",
            vec!["cargo xtask test run --tier boundary"],
        ),
        (
            "test-guest",
            vec![
                "cargo xtask fixtures fetch --tier guest",
                "cargo xtask test run --tier guest",
            ],
        ),
        (
            "test-artifact",
            vec!["cargo xtask test run --tier artifact"],
        ),
        ("gates", vec!["cargo xtask gates check"]),
    ]
}
fn switch_findings(root: &Path, m: &manifest::Manifest) -> Result<Vec<String>> {
    // Conservative lexical scan: every production source occurrence must be reviewed.
    // cfg-test-only names are reported as migration gaps rather than silently granted.
    let mut actual = BTreeSet::new();
    let mut paths = super::discovery::files(&root.join("src"))?;
    for entry in std::fs::read_dir(root.join("crates"))? {
        paths.extend(super::discovery::files(&entry?.path().join("src"))?);
    }
    for path in paths {
        let text = std::fs::read_to_string(path)?;
        for (_, tail) in text
            .match_indices("AWMAN_TEST_")
            .map(|(i, _)| (i, &text[i..]))
        {
            actual.insert(
                tail.chars()
                    .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                    .collect::<String>(),
            );
        }
    }
    let declared: BTreeSet<_> = m
        .shipped_switch
        .iter()
        .map(|s| s.name.clone())
        .chain(m.test_env.values().cloned())
        .collect();
    Ok(actual.difference(&declared).map(|name|format!("unreviewed AWMAN_TEST switch/environment literal {name}: production/cfg classification required")).collect())
}

/// A second, reviewed record prevents silently shrinking a release-critical scenario.
pub fn scenario_findings(root: &Path, m: &manifest::Manifest) -> Vec<String> {
    let path = root.join("ci/release-critical-tests.json");
    let expected: std::collections::BTreeMap<String, Vec<String>> = match std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(v) => v,
        None => return vec!["missing or invalid release-critical test-set contract".into()],
    };
    let actual: std::collections::BTreeMap<_, _> = m
        .release_critical
        .iter()
        .map(|s| {
            let mut tests = s.tests.clone();
            tests.sort();
            (s.id.clone(), tests)
        })
        .collect();
    if actual == expected {
        vec![]
    } else {
        vec!["release-critical test-set contract changed; reconcile ci/release-critical-tests.json in review".into()]
    }
}
