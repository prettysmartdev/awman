//! Seeded defects (nightly): proof that the test system can fail.
//!
//! * `source`: every `ci/seeded-defects/<layer>/*.patch` is applied to the work
//!   tree, the suites it names must go red (exit 1: exit 2 means "could not
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
        !expect.is_empty(),
        "{}: missing `# expect-red:` header",
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
    let mut out = Vec::new();
    for layer in std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.path().is_dir())
    {
        let layer_name = layer.file_name().to_string_lossy().into_owned();
        for p in std::fs::read_dir(layer.path())?.flatten() {
            let path = p.path();
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

fn source(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["dir", "report"], &[])?;
    let dir = root.join(a.get("dir").unwrap_or("ci/seeded-defects"));
    let patches = find_patches(&dir)?;
    anyhow::ensure!(
        !patches.is_empty(),
        "no seeded-defect patches under {}: the check would prove nothing",
        dir.display()
    );
    let exe = std::env::current_exe()?;
    let mut report = String::from(
        "## Seeded source defects\n\n| patch | layer | result |\n| --- | --- | --- |\n",
    );
    let mut ok = true;
    for p in &patches {
        let patch = p.path.to_string_lossy().into_owned();
        let result = if !git(root, &["apply", "--check", &patch])? {
            ok = false;
            "STALE: the patch no longer applies".to_string()
        } else {
            git(root, &["apply", &patch])?;
            let exits: Vec<(String, i32)> = p
                .expect_red
                .iter()
                .map(|suite| {
                    let code = Command::new(&exe)
                        .current_dir(root)
                        .args([
                            "test",
                            "run",
                            "--suite",
                            suite,
                            "--build",
                            "linux-x64-default",
                        ])
                        .status()
                        .map(|s| s.code().unwrap_or(2))
                        .unwrap_or(2);
                    (suite.clone(), code)
                })
                .collect();
            let reverted = git(root, &["apply", "-R", &patch])?;
            anyhow::ensure!(
                reverted,
                "could not revert {}; the work tree is dirty",
                patch
            );
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
        report.push_str(&format!(
            "| {} ({}) | {} | {result} |\n",
            p.path.file_name().unwrap().to_string_lossy(),
            p.defect,
            p.layer
        ));
    }
    if let Some(r) = a.get("report") {
        std::fs::write(r, &report)?;
    }
    print!("{report}");
    verdict(ok)
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

    // 3. Hardware cells are silently skipped: the plan must show them blocked, never absent.
    let request = plan::Request {
        repository_id: "1".into(),
        event: "push".into(),
        head_sha: "a".repeat(40),
        base_sha: None,
        paths: vec![],
        diff_complete: true,
        admitted_platforms: Default::default(),
        suite_durations_ms: Default::default(),
        suite_test_counts: Default::default(),
        main_durations_ms: vec![],
    };
    let p = plan::create(m, "main", request, today)?;
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
        _ => bail!("usage: ci seeded-defects pipeline | source [--dir D] [--report FILE]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_required_and_parsed() {
        let ok = parse_header(Path::new("x.patch"), "layer0", "# defect: flips a bound\n# expect-red: unit-awman, component-data\ndiff --git a/x b/x\n").unwrap();
        assert_eq!(ok.expect_red, vec!["unit-awman", "component-data"]);
        assert!(parse_header(Path::new("x.patch"), "l", "diff --git a/x b/x\n").is_err());
        assert!(parse_header(Path::new("x.patch"), "l", "# defect: d\n").is_err());
    }

    #[test]
    fn only_exit_one_counts_as_red() {
        let s = |n: &str, c| (n.to_string(), c);
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
