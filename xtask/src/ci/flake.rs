//! Flake report over repeated runs of the same archive. A test that both passed
//! and failed across runs is intermittent; one that failed every time is simply
//! broken. Either makes the nightly red and the report becomes the issue body.

use super::{args::Args, verdict};
use crate::evidence::{emit::read_directory, schema::Outcome};
use anyhow::Result;
use std::{collections::BTreeMap, path::Path, process::ExitCode};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub passed: u32,
    pub failed: u32,
    pub runs_failed: Vec<String>,
}

/// `(run name, test, outcome)` -> verdict per test.
pub fn tally(samples: &[(String, String, Outcome)]) -> BTreeMap<String, Tally> {
    let mut out: BTreeMap<String, Tally> = BTreeMap::new();
    for (run, test, outcome) in samples {
        let t = out.entry(test.clone()).or_default();
        match outcome {
            Outcome::Passed => t.passed += 1,
            Outcome::Failed => {
                t.failed += 1;
                t.runs_failed.push(run.clone());
            }
            _ => {}
        }
    }
    out
}

pub fn report(tally: &BTreeMap<String, Tally>, seeds: &str, runs: usize) -> (String, bool) {
    let flaky: Vec<_> = tally
        .iter()
        .filter(|(_, t)| t.passed > 0 && t.failed > 0)
        .collect();
    let broken: Vec<_> = tally
        .iter()
        .filter(|(_, t)| t.passed == 0 && t.failed > 0)
        .collect();
    let mut md = format!(
        "## Flake hunt: {runs} repeated run(s)\n\nShuffle seeds:\n\n```\n{}\n```\n\n",
        seeds.trim()
    );
    if flaky.is_empty() && broken.is_empty() {
        md.push_str("No intermittent or failing tests.\n");
    }
    for (title, list) in [
        ("Intermittent (passed and failed)", &flaky),
        ("Failed in every run", &broken),
    ] {
        if list.is_empty() {
            continue;
        }
        md.push_str(&format!("### {title}\n\n"));
        for (test, t) in list.iter() {
            md.push_str(&format!(
                "- `{test}`: {} passed, {} failed (failed in {})\n",
                t.passed,
                t.failed,
                t.runs_failed.join(", ")
            ));
        }
        md.push('\n');
    }
    (md, flaky.is_empty() && broken.is_empty())
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["in", "out"], &[])?;
    let dir = Path::new(a.need("in")?);
    let mut samples = Vec::new();
    let mut runs = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut names: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            runs += 1;
            let run = e.file_name().to_string_lossy().into_owned();
            for ev in read_directory(&e.path())? {
                for r in ev.results {
                    samples.push((run.clone(), format!("{}::{}", ev.suite, r.test), r.outcome));
                }
            }
        }
    }
    anyhow::ensure!(
        runs > 0,
        "no run directories under {}: the hunt produced no evidence",
        dir.display()
    );
    let seeds = std::fs::read_to_string(dir.join("seeds.txt")).unwrap_or_default();
    let (md, ok) = report(&tally(&samples), &seeds, runs);
    std::fs::write(a.need("out")?, &md)?;
    print!("{md}");
    verdict(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(run: &str, test: &str, o: Outcome) -> (String, String, Outcome) {
        (run.into(), test.into(), o)
    }

    #[test]
    fn mixed_outcomes_are_intermittent_and_red() {
        let t = tally(&[
            s("run1", "a", Outcome::Passed),
            s("run2", "a", Outcome::Failed),
            s("run1", "b", Outcome::Passed),
            s("run2", "b", Outcome::Passed),
        ]);
        let (md, ok) = report(&t, "run 1 seed 7", 2);
        assert!(!ok);
        assert!(md.contains("Intermittent") && md.contains("`a`") && !md.contains("`b`"));
        assert!(md.contains("seed 7"));
    }

    #[test]
    fn consistent_failure_is_reported_and_clean_runs_are_green() {
        let t = tally(&[
            s("run1", "c", Outcome::Failed),
            s("run2", "c", Outcome::Failed),
        ]);
        let (md, ok) = report(&t, "", 2);
        assert!(!ok && md.contains("Failed in every run"));
        let (_, ok) = report(&tally(&[s("run1", "d", Outcome::Passed)]), "", 1);
        assert!(ok);
    }

    #[test]
    fn an_empty_hunt_is_an_error_not_a_pass() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("r.md");
        assert!(run(&[
            "--in".into(),
            d.path().to_string_lossy().into_owned(),
            "--out".into(),
            out.to_string_lossy().into_owned()
        ])
        .is_err());
    }
}
