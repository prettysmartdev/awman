//! `cargo xtask matrix`: render and check the acceptance matrix (design 8.7).
//!
//! `ci/acceptance-matrix.toml` has one `[[cell]]` per subsystem × mode ×
//! backend × runtime × platform × feature:
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
    problems.extend(template_findings(root, &subsystems)?);
    for asset in &manifest.release.assets {
        if !subsystems.contains(format!("release {asset}").as_str()) {
            problems.push(format!("{MATRIX}: no row for release asset {asset}"));
        }
    }
    for path in catalogue_paths(root)? {
        if !subsystems.contains(format!("command {path}").as_str()) {
            problems.push(format!("{MATRIX}: no row for catalogue leaf {path}"));
        }
    }
    for gate in manifest.gates.keys() {
        for cell in crate::manifest::validate::cells(&manifest, gate)? {
            if let Some(suite) = manifest.suites.get(&cell.suite) {
                let feature = &manifest.builds[&cell.build].features;
                if !matrix.cell.iter().any(|c| {
                    c.subsystem == format!("suite {}", cell.suite)
                        && c.platform == cell.platform
                        && c.feature == *feature
                        && c.backend == suite.backend.as_deref().unwrap_or("n/a")
                }) {
                    problems.push(format!(
                        "{MATRIX}: missing suite/platform/backend cell {} on {}",
                        cell.id, cell.platform
                    ));
                }
            }
        }
    }
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

fn template_findings(root: &Path, subsystems: &BTreeSet<&str>) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(root.join("templates"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type()?.is_file() && !subsystems.contains(format!("template {name}").as_str())
        {
            problems.push(format!("{MATRIX}: no row for shipped template {name}"));
        }
    }
    Ok(problems)
}
fn catalogue_paths(root: &Path) -> Result<Vec<String>> {
    use syn::visit::Visit;
    struct Strings(Vec<String>);
    impl<'ast> Visit<'ast> for Strings {
        fn visit_lit_str(&mut self, value: &'ast syn::LitStr) {
            self.0.push(value.value());
        }
    }
    let source = std::fs::read_to_string(root.join("tests/component/command/catalogue.rs"))?;
    let syntax = syn::parse_file(&source)?;
    let contract = syntax
        .items
        .iter()
        .find_map(|i| match i {
            syn::Item::Const(c) if c.ident == "CONTRACT_PATHS" => Some(c),
            _ => None,
        })
        .context("missing independent catalogue leaf contract")?;
    let mut found = Strings(Vec::new());
    found.visit_expr(&contract.expr);
    anyhow::ensure!(!found.0.is_empty(), "empty catalogue leaf contract");
    Ok(found.0)
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
    fn non_docker_templates_cannot_disappear_from_the_inventory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("templates")).unwrap();
        std::fs::write(root.path().join("templates/sbx-kit.example.yaml"), "kit").unwrap();
        std::fs::write(root.path().join("templates/agent-definition.toml"), "agent").unwrap();
        let incomplete = BTreeSet::from(["template sbx-kit.example.yaml"]);
        assert!(template_findings(root.path(), &incomplete)
            .unwrap()
            .iter()
            .any(|p| p.contains("agent-definition.toml")));
        let complete = BTreeSet::from([
            "template sbx-kit.example.yaml",
            "template agent-definition.toml",
        ]);
        assert!(template_findings(root.path(), &complete)
            .unwrap()
            .is_empty());
    }
}
