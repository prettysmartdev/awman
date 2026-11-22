//! Seeded defects (nightly): proof that the test system can fail.
//!
//! * `source`: every `ci/seeded-defects/<layer>/*.patch` is applied to an owned
//!   isolated checkout; its suites must go red (exit 1: exit 2 means "could not
//!   run" and proves nothing), then it is reverted. A patch that no longer
//!   applies, or whose suites stay green, fails the run: a seeded defect that
//!   the tests miss is a hole in the tests, a stale patch is a hole in this
//!   check. A directory without patches also fails.
//! * `pipeline`: the three pipeline faults (missing target, renamed job,
//!   skipped hardware) against the pure gate APIs; each must be detected.
//!
//! Patch header (ignored by `git apply`):
//!
//! ```text
//! # defect: what was broken
//! # expect-red: unit-awman,component-data
//! ```

use super::{args::Args, verdict};
use crate::{
    gates::{plan, render::workflows},
    manifest::{self, Manifest},
};
use anyhow::{bail, Context, Result};
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Patch {
    pub path: PathBuf,
    pub layer: String,
    pub defect: String,
    pub expect_red: Vec<String>,
}

pub fn parse_header(path: &Path, layer: &str, text: &str) -> Result<Patch> {
    let mut defect = None;
    let mut expect = Vec::new();
    for line in text
        .lines()
        .take_while(|l| l.starts_with('#') || l.trim().is_empty())
    {
        if let Some(d) = line.strip_prefix("# defect:") {
            defect = Some(d.trim().to_owned());
        }
        if let Some(e) = line.strip_prefix("# expect-red:") {
            expect = e
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    let defect =
        defect.with_context(|| format!("{}: missing `# defect:` header", path.display()))?;
    anyhow::ensure!(
        !defect.is_empty(),
        "{}: empty defect description",
        path.display()
    );
    anyhow::ensure!(
        !expect.is_empty(),
        "{}: missing `# expect-red:` header",
        path.display()
    );
    anyhow::ensure!(
        expect
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == expect.len(),
        "{}: duplicate expect-red suite",
        path.display()
    );
    anyhow::ensure!(
        !text.contains("#[cfg(test)]")
            && !text.contains("#[test]")
            && !text.contains("#[tokio::test"),
        "{}: seed hunks may not edit tests",
        path.display()
    );
    let mut paths = 0;
    for line in text.lines().filter_map(|l| l.strip_prefix("+++ b/")) {
        anyhow::ensure!(
            Path::new(line)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
                && line.ends_with(".rs")
                && (line.starts_with("src/")
                    || line.starts_with("xtask/src/")
                    || (line.starts_with("crates/") && line.contains("/src/"))),
            "{}: seeded defects must change production Rust source",
            path.display()
        );
        paths += 1;
    }
    anyhow::ensure!(
        paths > 0
            && text.lines().any(|l| l.starts_with("@@ "))
            && text
                .lines()
                .any(|l| l.starts_with('+') && !l.starts_with("+++"))
            && text
                .lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---")),
        "{}: empty source defect patch",
        path.display()
    );
    Ok(Patch {
        path: path.into(),
        layer: layer.into(),
        defect,
        expect_red: expect,
    })
}

pub fn find_patches(dir: &Path) -> Result<Vec<Patch>> {
    fn check_layout(root: &Path, dir: &Path) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                check_layout(root, &path)?;
            } else if path.extension().is_some_and(|e| e == "patch") {
                anyhow::ensure!(
                    path.strip_prefix(root)?.components().count() == 2,
                    "stray seeded-defect patch {} (expected <layer>/*.patch)",
                    path.display()
                );
            }
        }
        Ok(())
    }
    check_layout(dir, dir)?;
    let mut out = Vec::new();
    for layer in std::fs::read_dir(dir)? {
        let layer = layer?;
        if !layer.file_type()?.is_dir() {
            continue;
        }
        let layer_name = layer.file_name().to_string_lossy().into_owned();
        for p in std::fs::read_dir(layer.path())? {
            let path = p?.path();
            if path.extension().is_some_and(|e| e == "patch") {
                out.push(parse_header(
                    &path,
                    &layer_name,
                    &std::fs::read_to_string(&path)?,
                )?);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Red,
    Survived(Vec<String>),
    Inconclusive(Vec<String>),
}

/// `exits`: `(suite, exit code)` for every expected suite.
pub fn classify(exits: &[(String, i32)]) -> Status {
    if exits.is_empty() {
        return Status::Inconclusive(vec!["no expected suites".into()]);
    }
    let survived: Vec<_> = exits
        .iter()
        .filter(|(_, c)| *c == 0)
        .map(|(s, _)| s.clone())
        .collect();
    let inconclusive: Vec<_> = exits
        .iter()
        .filter(|(_, c)| *c != 0 && *c != 1)
        .map(|(s, _)| s.clone())
        .collect();
    if !survived.is_empty() {
        Status::Survived(survived)
    } else if !inconclusive.is_empty() {
        Status::Inconclusive(inconclusive)
    } else {
        Status::Red
    }
}

fn git(root: &Path, args: &[&str]) -> Result<bool> {
    Ok(Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .context("git")?
        .success())
}

/// Only remove the worktree this invocation allocated. Explicit cleanup makes
/// cleanup failure non-passing; Drop also handles errors on intermediate steps.
struct Checkout {
    owner: PathBuf,
    path: PathBuf,
    _temp: tempfile::TempDir,
    removed: bool,
}
impl Checkout {
    fn new(root: &Path) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("checkout");
        anyhow::ensure!(
            git(
                root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    path.to_str().context("worktree path")?,
                    "HEAD"
                ]
            )?,
            "could not create an isolated seeded-defect checkout"
        );
        Ok(Self {
            owner: root.into(),
            path,
            _temp: temp,
            removed: false,
        })
    }
    fn close(&mut self) -> Result<()> {
        anyhow::ensure!(
            git(
                &self.owner,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    self.path.to_str().context("worktree path")?
                ]
            )?,
            "could not remove owned seeded-defect checkout {}",
            self.path.display()
        );
        self.removed = true;
        Ok(())
    }
}
impl Drop for Checkout {
    fn drop(&mut self) {
        if !self.removed {
            let _ = self.close();
        }
    }
}

/// Exit 1 proves detection only when fresh JUnit reports a product-test failure.
/// Missing reports, compiler/startup failures and blocked/skipped tests prove
/// nothing, even when an outer runner returns the same nonzero exit code.
fn suite_result(code: i32, evidence: &Path) -> Result<i32> {
    use crate::evidence::schema::Outcome;
    let mut results = Vec::new();
    for entry in std::fs::read_dir(evidence)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "xml") {
            results.extend(crate::evidence::junit::parse(&std::fs::read_to_string(
                path,
            )?)?);
        }
    }
    if results.is_empty()
        || results
            .iter()
            .any(|r| !matches!(r.outcome, Outcome::Passed | Outcome::Failed))
    {
        return Ok(2);
    }
    let failed = results.iter().any(|r| r.outcome == Outcome::Failed);
    Ok(match (code, failed) {
        (0, false) => 0,
        (1, true) => 1,
        _ => 2,
    })
}

fn run_suite(
    exe: &Path,
    checkout: &Path,
    tools: &Path,
    build: &str,
    suite: &str,
    evidence: &Path,
) -> Result<i32> {
    std::fs::create_dir_all(evidence)?;
    let mut paths = vec![tools.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let status = Command::new(exe)
        .current_dir(checkout)
        .env("CARGO_MANIFEST_DIR", checkout.join("xtask"))
        .env("CARGO_TARGET_DIR", checkout.join("target"))
        .env("PATH", std::env::join_paths(paths)?)
        .args([
            "test",
            "run",
            "--suite",
            suite,
            "--build",
            build,
            "--evidence-dir",
        ])
        .arg(evidence)
        .stdout(std::fs::File::create(evidence.join("stdout.log"))?)
        .stderr(std::fs::File::create(evidence.join("stderr.log"))?)
        .status()?;
    // Keep raw evidence on parsing failure; it cannot establish detection.
    Ok(suite_result(status.code().unwrap_or(2), evidence).unwrap_or(2))
}

fn source(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["dir", "report", "build"], &[])?;
    let dir = root.join(a.get("dir").unwrap_or("ci/seeded-defects"));
    let patches = find_patches(&dir)?;
    anyhow::ensure!(
        !patches.is_empty(),
        "no seeded-defect patches under {}: the check would prove nothing",
        dir.display()
    );
    let report_path = std::path::absolute(a.get("report").unwrap_or("seeded-defects.md"))?;
    let evidence = report_path.with_extension("evidence");
    // Do not reuse previous JUnit or patch results on a rerun.
    anyhow::ensure!(
        !evidence.exists(),
        "seeded-defect evidence already exists: {}",
        evidence.display()
    );
    std::fs::create_dir_all(&evidence)?;
    let mut report = String::from(
        "## Seeded source defects\n\n| patch | layer | result |\n| --- | --- | --- |\n",
    );
    let result = (|| -> Result<bool> {
        let mut checkout = Checkout::new(root)?;
        let m = manifest::load(&checkout.path)?;
        let build = a.get("build").unwrap_or("linux-x64-default");
        anyhow::ensure!(
            m.builds.contains_key(build),
            "unknown seeded-defect build {build}"
        );
        let suites: std::collections::BTreeSet<_> =
            patches.iter().flat_map(|p| p.expect_red.iter()).collect();
        for suite in &suites {
            let s = m
                .suites
                .get(*suite)
                .with_context(|| format!("unregistered expect-red suite {suite}"))?;
            anyhow::ensure!(
                s.kind == "nextest",
                "expect-red suite {suite} must provide nextest test evidence"
            );
        }
        std::fs::write(
            evidence.join("baseline-source.json"),
            serde_json::to_vec_pretty(&crate::evidence::actions::source(&checkout.path)?)?,
        )?;
        let exe = std::env::current_exe()?;
        let tools = root.join("target/xtask-tools/bin");
        for suite in suites {
            let code = run_suite(
                &exe,
                &checkout.path,
                &tools,
                build,
                suite,
                &evidence.join("baseline").join(suite),
            )?;
            anyhow::ensure!(
                code == 0,
                "baseline suite {suite} did not pass; see {}",
                evidence.display()
            );
        }
        let mut ok = true;
        for (index, p) in patches.iter().enumerate() {
            validate_production_hunks(&checkout.path, &std::fs::read_to_string(&p.path)?)?;
            let patch = std::fs::canonicalize(&p.path)?;
            let patch = patch.to_str().context("patch path")?;
            let patch_evidence = evidence.join(format!("patch-{index}"));
            std::fs::create_dir_all(&patch_evidence)?;
            let result = if !git(&checkout.path, &["apply", "--check", patch])? {
                ok = false;
                "STALE: the patch no longer applies".to_string()
            } else {
                anyhow::ensure!(
                    git(&checkout.path, &["apply", patch])?,
                    "could not apply {patch}"
                );
                anyhow::ensure!(
                    !git(&checkout.path, &["diff", "--quiet", "HEAD"])?,
                    "empty patch {patch}"
                );
                let exits: Vec<(String, i32)> = p
                    .expect_red
                    .iter()
                    .map(|suite| {
                        let code = run_suite(
                            &exe,
                            &checkout.path,
                            &tools,
                            build,
                            suite,
                            &patch_evidence.join(suite),
                        )
                        .unwrap_or(2);
                        (suite.clone(), code)
                    })
                    .collect();
                let reverted = git(&checkout.path, &["apply", "-R", patch])?
                    && git(&checkout.path, &["diff", "--quiet", "HEAD"])?;
                anyhow::ensure!(
                    reverted,
                    "could not restore isolated baseline after {}",
                    patch
                );
                std::fs::write(
                    patch_evidence.join("results.json"),
                    serde_json::to_vec_pretty(&exits)?,
                )?;
                // A defect is detected only if the same declared suites pass
                // again after restoration, with fresh JUnit evidence.
                let mut restored = true;
                for suite in &p.expect_red {
                    restored &= run_suite(
                        &exe,
                        &checkout.path,
                        &tools,
                        build,
                        suite,
                        &patch_evidence.join("restored").join(suite),
                    )? == 0;
                }
                let exits = if restored {
                    exits
                } else {
                    exits.into_iter().map(|(s, _)| (s, 2)).collect()
                };
                match classify(&exits) {
                    Status::Red => "red, as required".into(),
                    Status::Survived(s) => {
                        ok = false;
                        format!("SURVIVED: {} stayed green", s.join(", "))
                    }
                    Status::Inconclusive(s) => {
                        ok = false;
                        format!("INCONCLUSIVE: {} could not run", s.join(", "))
                    }
                }
            };
            std::fs::write(
                patch_evidence.join("patch.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "path": p.path, "sha256": crate::evidence::digest::sha256_file(&p.path)?,
                    "defect": p.defect, "expect_red": p.expect_red, "result": result
                }))?,
            )?;
            report.push_str(&format!(
                "| {} ({}) | {} | {result} |\n",
                p.path.file_name().unwrap().to_string_lossy(),
                p.defect,
                p.layer
            ));
        }
        checkout.close()?;
        Ok(ok)
    })();
    if let Err(e) = &result {
        report.push_str(&format!("\nINCONCLUSIVE: {e:#}\n"));
    }
    std::fs::write(&report_path, &report)?;
    print!("{report}");
    verdict(result?)
}

#[derive(Debug)]
pub struct Fault {
    pub name: &'static str,
    pub detected: bool,
    pub detail: String,
}

/// The three pipeline faults of design 11.4.
pub fn pipeline_faults(m: &Manifest) -> Result<Vec<Fault>> {
    let today = chrono::Utc::now().date_naive();
    let mut faults = Vec::new();

    // 1. A required target disappears from the manifest.
    let mut missing = m.clone();
    let group = missing
        .gates
        .get_mut("pr")
        .and_then(|g| g.cells.iter_mut().find(|c| c.builds.len() > 1))
        .context("pr gate has no multi-build group")?;
    let removed = group.builds.pop().unwrap_or_default();
    let verdict = manifest::validate::validate(&missing, today);
    faults.push(Fault {
        name: "missing target",
        detected: verdict.is_err(),
        detail: format!(
            "removed build {removed} from a pr cell group: {}",
            verdict
                .err()
                .map_or("validation passed".into(), |e| format!("{e:#}"))
        ),
    });

    // 2. A required job is renamed in the workflow.
    let rendered = workflows::outputs(m)?;
    let (path, ci) = rendered
        .iter()
        .find(|(p, _)| p.ends_with("ci.yml"))
        .context("ci.yml not rendered")?;
    let renamed = ci.replacen("\n  gate:\n", "\n  final:\n", 1);
    let wf = m.workflows.get(path).context("ci.yml not in [workflows]")?;
    let errors = workflows::verify_text(path, &renamed, wf);
    faults.push(Fault {
        name: "renamed job",
        detected: renamed != *ci
            && errors
                .iter()
                .any(|e| e.contains("missing or renamed required job gate")),
        detail: errors.join("; "),
    });

    // 3. Release hardware cells are silently skipped: the candidate plan must
    // show them blocked, never absent.
    let request = plan::Request {
        repository_id: "1".into(),
        event: "workflow_dispatch".into(),
        head_sha: "a".repeat(40),
        base_sha: None,
        paths: vec![],
        diff_complete: true,
        admitted_platforms: Default::default(),
        suite_durations_ms: Default::default(),
        suite_test_counts: Default::default(),
        main_durations_ms: vec![],
    };
    let p = plan::create(m, "candidate", request, today)?;
    let guest: Vec<_> = p.tests.iter().filter(|t| t.tier == "guest").collect();
    faults.push(Fault {
        name: "skipped hardware",
        detected: !guest.is_empty()
            && guest
                .iter()
                .all(|t| t.blocked_reason.is_some() && !t.self_hosted),
        detail: format!(
            "{} guest test entries, all blocked and hosted: {}",
            guest.len(),
            guest.iter().all(|t| t.blocked_reason.is_some())
        ),
    });
    Ok(faults)
}

fn pipeline(root: &Path) -> Result<ExitCode> {
    let faults = pipeline_faults(&manifest::load(root)?)?;
    let mut ok = true;
    for f in &faults {
        println!(
            "{}: {} ({})",
            f.name,
            if f.detected {
                "detected"
            } else {
                "NOT DETECTED"
            },
            f.detail
        );
        ok &= f.detected;
    }
    verdict(ok)
}

pub fn run(root: &Path, args: &[String]) -> Result<ExitCode> {
    match args.first().map(String::as_str) {
        Some("pipeline") => pipeline(root),
        Some("source") => source(root, &args[1..]),
        _ => bail!(
            "usage: ci seeded-defects pipeline | source [--dir D] [--report FILE] [--build ID]"
        ),
    }
}

/// Conservative source boundary: inline test modules live after cfg(test).
/// Hunk offsets refer to the clean checkout, so omitted patch context cannot
/// disguise an assertion edit deep inside such a module.
pub fn validate_production_hunks(root: &Path, patch: &str) -> Result<()> {
    let mut test_start = None;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            let source = std::fs::read_to_string(root.join(path))?;
            test_start = source
                .lines()
                .position(|line| line.trim() == "#[cfg(test)]")
                .map(|n| n + 1);
        }
        if let Some(hunk) = line.strip_prefix("@@ -") {
            let range = hunk.split_whitespace().next().context("invalid hunk")?;
            let mut parts = range.split(',');
            let start: usize = parts.next().context("missing old offset")?.parse()?;
            let count: usize = parts.next().unwrap_or("1").parse()?;
            anyhow::ensure!(
                test_start.is_none_or(|test| start.saturating_add(count) <= test),
                "seed hunk edits an inline test module"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stray_seed_patches_and_inline_test_hunks_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let seeds = dir.path().join("seeds");
        std::fs::create_dir_all(&seeds).unwrap();
        std::fs::write(seeds.join("stray.patch"), "bad").unwrap();
        assert!(find_patches(&seeds)
            .unwrap_err()
            .to_string()
            .contains("stray"));
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/sample.rs"),
            "fn production() {}\n#[cfg(test)]\nmod tests {\nfn assertion() {}\n}\n",
        )
        .unwrap();
        let patch = "+++ b/src/sample.rs\n@@ -4,1 +4,1 @@\n-fn assertion() {}\n+fn assertion() {panic!()}\n";
        assert!(validate_production_hunks(dir.path(), patch)
            .unwrap_err()
            .to_string()
            .contains("test module"));
    }
    #[test]
    fn header_is_required_and_parsed() {
        let ok = parse_header(Path::new("x.patch"), "layer0", "# defect: flips a bound\n# expect-red: unit-awman, component-data\ndiff --git a/src/x.rs b/src/x.rs\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1 +1 @@\n-before\n+after\n").unwrap();
        assert_eq!(ok.expect_red, vec!["unit-awman", "component-data"]);
        assert!(parse_header(Path::new("x.patch"), "l", "diff --git a/x b/x\n").is_err());
        assert!(parse_header(Path::new("x.patch"), "l", "# defect: d\n").is_err());
    }

    #[test]
    fn only_exit_one_counts_as_red() {
        let s = |n: &str, c| (n.to_string(), c);
        assert!(matches!(classify(&[]), Status::Inconclusive(_)));
        assert_eq!(classify(&[s("a", 1), s("b", 1)]), Status::Red);
        assert_eq!(
            classify(&[s("a", 1), s("b", 0)]),
            Status::Survived(vec!["b".into()])
        );
        assert_eq!(
            classify(&[s("a", 2)]),
            Status::Inconclusive(vec!["a".into()])
        );
        assert_eq!(
            classify(&[s("a", 0), s("b", 2)]),
            Status::Survived(vec!["a".into()])
        );
    }

    #[test]
    fn compiler_blocked_skipped_or_missing_junit_is_not_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(suite_result(1, dir.path()).unwrap(), 2);
        for (body, code, expected) in [
            ("", 0, 0),
            ("<failure message=\"assertion failed\"/>", 1, 1),
            ("<failure message=\"assertion failed\"/>", 2, 2),
            ("<failure message=\"BLOCKED[hypervisor]\"/>", 1, 2),
            ("<skipped/>", 1, 2),
        ] {
            std::fs::write(dir.path().join("current.xml"), format!("<testsuite><testcase classname=\"pkg::lib\" name=\"test\">{body}</testcase></testsuite>")).unwrap();
            assert_eq!(suite_result(code, dir.path()).unwrap(), expected);
        }
    }

    #[test]
    fn an_empty_patch_directory_is_a_failure() {
        let d = tempfile::tempdir().unwrap();
        assert!(source(d.path(), &[]).is_err());
    }

    #[test]
    fn every_pipeline_fault_is_detected_by_the_real_manifest() {
        let m = manifest::load(&crate::gates::root()).unwrap();
        for f in pipeline_faults(&m).unwrap() {
            assert!(f.detected, "{} was not detected: {}", f.name, f.detail);
        }
    }
}
