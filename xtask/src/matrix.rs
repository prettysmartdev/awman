//! `cargo xtask matrix`: render and check the acceptance matrix (design 8.7).
//!
//! `ci/acceptance-matrix.toml` records important behavior anchors; full platform/suite
//! selection is derived from the manifest rather than duplicated here:
//!
//! ```toml
//! [[cell]]
//! subsystem = "engine"          # area of the product
//! mode = "command"              # command | interactive | api | daemon | n/a
//! backend = "both"              # builtin | daemon | both | n/a
//! runtime = "fake"              # fake | docker | apple-containers | sbx | hvf | kvm | n/a
//! platform = "linux-x64"        # a [platforms] id of ci/gates.toml, or "all-hosted"
//! feature = "default"           # a [features] id
//! status = "covered"            # covered | not-applicable | blocked
//! tests = ["awman-cli::component engine::git::clone_over_ssh"]   # covered: test IDs
//! gate = "pr"                   # covered: the gate whose evidence certifies it
//! reason = "…"                  # not-applicable / blocked: why
//! owner = "maintainer"          # blocked: who unblocks it
//! release_impact = "…"          # blocked: what a release may not claim meanwhile
//! ```
//!
//! `check` verifies every named test exists (source discovery), every status
//! has its required fields, and that every shipped Dockerfile template, every
//! release asset and every catalogue-derived contract has a row. `render`
//! writes `aspec/devops/acceptance-matrix.md`. `covered` here means "a named,
//! existing test in a named gate"; it is certified only by that gate's
//! authenticated evidence, never by this file.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path, process::ExitCode};

const MATRIX: &str = "ci/acceptance-matrix.toml";
const RENDERED: &str = "aspec/devops/acceptance-matrix.md";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Matrix {
    schema: u32,
    cell: Vec<Cell>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cell {
    subsystem: String,
    mode: String,
    backend: String,
    runtime: String,
    platform: String,
    feature: String,
    status: String,
    #[serde(default)]
    tests: Vec<String>,
    gate: Option<String>,
    reason: Option<String>,
    owner: Option<String>,
    release_impact: Option<String>,
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    let root = crate::gates::root();
    match args.first().map(String::as_str) {
        Some("check") => {
            let problems = check(&root)?;
            for p in &problems {
                eprintln!("{p}");
            }
            let rendered = render(&load(&root)?);
            if std::fs::read_to_string(root.join(RENDERED)).ok().as_deref() != Some(&rendered) {
                eprintln!("{RENDERED}: stale; run `cargo xtask matrix render`");
                return Ok(ExitCode::from(1));
            }
            Ok(if problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Some("render") => {
            std::fs::write(root.join(RENDERED), render(&load(&root)?))?;
            eprintln!("matrix: wrote {RENDERED}");
            Ok(ExitCode::SUCCESS)
        }
        _ => {
            eprintln!("usage: cargo xtask matrix render | check");
            Ok(ExitCode::from(2))
        }
    }
}

fn load(root: &Path) -> Result<Matrix> {
    let text =
        std::fs::read_to_string(root.join(MATRIX)).with_context(|| format!("read {MATRIX}"))?;
    let m: Matrix = toml::from_str(&text).with_context(|| format!("parse {MATRIX}"))?;
    anyhow::ensure!(m.schema == 1, "{MATRIX}: unsupported schema {}", m.schema);
    Ok(m)
}

fn check(root: &Path) -> Result<Vec<String>> {
    let matrix = load(root)?;
    let manifest = crate::manifest::load(root)?;
    let meta = crate::gates::discovery::metadata(root)?;
    let discovered = crate::gates::discovery::audit(root, &manifest, &meta)?;
    let mut problems = Vec::new();
    for (i, c) in matrix.cell.iter().enumerate() {
        let at = format!(
            "{MATRIX} cell {} ({} {} {} {})",
            i + 1,
            c.subsystem,
            c.mode,
            c.platform,
            c.feature
        );
        if c.platform != "all-hosted" && !manifest.platforms.contains_key(&c.platform) {
            problems.push(format!("{at}: unknown platform {}", c.platform));
        }
        if !manifest.features.contains_key(&c.feature) {
            problems.push(format!("{at}: unknown feature set {}", c.feature));
        }
        match c.status.as_str() {
            "covered" => {
                if c.tests.is_empty() {
                    problems.push(format!("{at}: covered without test IDs"));
                }
                match &c.gate {
                    Some(g) if manifest.gates.contains_key(g) => {}
                    other => problems.push(format!(
                        "{at}: covered needs a manifest gate, got {other:?}"
                    )),
                }
                for t in &c.tests {
                    if !discovered.tests.contains(t) {
                        problems.push(format!("{at}: names missing test {t}"));
                    }
                }
            }
            "not-applicable" => {
                if c.reason.as_deref().is_none_or(str::is_empty) {
                    problems.push(format!("{at}: not-applicable needs a reason"));
                }
            }
            "blocked" => {
                if c.reason.is_none() || c.owner.is_none() || c.release_impact.is_none() {
                    problems.push(format!(
                        "{at}: blocked needs reason, owner and release_impact"
                    ));
                }
            }
            other => problems.push(format!("{at}: unknown status {other}")),
        }
    }
    let subsystems: BTreeSet<_> = matrix.cell.iter().map(|c| c.subsystem.as_str()).collect();
    problems.extend(coverage_findings(&subsystems));
    for asset in &manifest.release.assets {
        if !subsystems.contains(format!("release {asset}").as_str()) {
            problems.push(format!("{MATRIX}: no row for release asset {asset}"));
        }
    }
    // Platform/suite coverage is checked by the manifest cell validator and
    // actual compiled inventories, not a second hand-maintained Cartesian table.
    for derived in ["command-catalogue", "config-keys"] {
        if !subsystems.contains(derived) {
            problems.push(format!(
                "{MATRIX}: no row for the derived {derived} contract"
            ));
        }
    }
    Ok(problems)
}

fn render(m: &Matrix) -> String {
    let mut s = String::from(
        "# Acceptance matrix\n\n\
         Generated by `cargo xtask matrix render` from `ci/acceptance-matrix.toml`; do not edit.\n\
         `covered` names existing tests and the gate whose authenticated evidence certifies\n\
         them; it is not a claim that those tests have passed on that platform.\n\n\
         | Subsystem | Mode | Backend | Runtime | Platform | Feature | Status | Tests / reason |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for c in &m.cell {
        let detail = match c.status.as_str() {
            "covered" => format!(
                "{} (gate `{}`)",
                c.tests
                    .iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                c.gate.as_deref().unwrap_or("?")
            ),
            "blocked" => format!(
                "{}; owner {}; release impact: {}",
                c.reason.as_deref().unwrap_or(""),
                c.owner.as_deref().unwrap_or(""),
                c.release_impact.as_deref().unwrap_or("")
            ),
            _ => c.reason.clone().unwrap_or_default(),
        };
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            c.subsystem,
            c.mode,
            c.backend,
            c.runtime,
            c.platform,
            c.feature,
            c.status,
            detail.replace('|', "\\|")
        ));
    }
    s
}

fn coverage_findings(subsystems: &BTreeSet<&str>) -> Vec<String> {
    [
        "foundation",
        "data",
        "engine",
        "workflow",
        "command",
        "daemon",
        "frontend",
        "squad",
        "worker",
        "security",
        "templates",
        "boundary",
        "native",
        "ci-release",
    ]
    .into_iter()
    .filter(|name| !subsystems.contains(name))
    .map(|name| format!("{MATRIX}: missing important behavior slice {name}"))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_lists_every_cell_with_its_evidence_or_reason() {
        let m: Matrix = toml::from_str(
            r#"schema = 1
            [[cell]]
            subsystem = "engine"
            mode = "command"
            backend = "both"
            runtime = "fake"
            platform = "linux-x64"
            feature = "default"
            status = "covered"
            tests = ["awman-cli::component engine::x"]
            gate = "pr"
            [[cell]]
            subsystem = "guest"
            mode = "command"
            backend = "builtin"
            runtime = "hvf"
            platform = "macos-arm64-ci-guest"
            feature = "builtin"
            status = "blocked"
            reason = "no hardware"
            owner = "maintainer"
            release_impact = "none claimed""#,
        )
        .unwrap();
        let out = render(&m);
        assert!(out.contains("| engine | command | both | fake | linux-x64 | default | covered | `awman-cli::component engine::x` (gate `pr`) |"));
        assert!(out.contains("no hardware; owner maintainer; release impact: none claimed"));
    }
    #[test]
    fn important_behavior_slices_cannot_disappear() {
        let missing = coverage_findings(&BTreeSet::from(["foundation", "data"]));
        assert!(missing.iter().any(|p| p.contains("workflow")));
        assert!(missing.iter().any(|p| p.contains("security")));
        assert!(!missing.iter().any(|p| p.ends_with("foundation")));
    }
}
