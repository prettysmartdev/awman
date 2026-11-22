use super::schema::{Outcome, ResultRecord};
use anyhow::{ensure, Result};
use std::collections::BTreeSet;
/// `classname` is the canonical package::target; `name` is the Rust module path.
/// IDs are matched to the independently produced expected set; no fuzzy matching.
pub fn parse(text: &str) -> Result<Vec<ResultRecord>> {
    let doc = roxmltree::Document::parse(text)?;
    let mut results = Vec::new();
    let mut ids = BTreeSet::new();
    for case in doc.descendants().filter(|n| n.has_tag_name("testcase")) {
        let class = case.attribute("classname").unwrap_or("");
        let name = case.attribute("name").unwrap_or("");
        ensure!(
            !class.is_empty() && !name.is_empty(),
            "JUnit testcase missing canonical classname/name"
        );
        let test = format!("{class} {name}");
        ensure!(ids.insert(test.clone()), "duplicate JUnit result {test}");
        let failure = case
            .children()
            .find(|n| n.has_tag_name("failure") || n.has_tag_name("error"));
        let skipped = case.children().find(|n| n.has_tag_name("skipped"));
        let detail = failure.or(skipped).map(|n| {
            format!(
                "{} {}",
                n.attribute("message").unwrap_or(""),
                n.text().unwrap_or("")
            )
        });
        let outcome = if skipped.is_some() {
            Outcome::Skipped
        } else if failure.is_some() {
            if detail
                .as_deref()
                .is_some_and(|s| s.trim_start().starts_with("BLOCKED["))
            {
                Outcome::Blocked
            } else {
                Outcome::Failed
            }
        } else {
            Outcome::Passed
        };
        let time = case.attribute("time").unwrap_or("0").parse::<f64>()?;
        ensure!(
            time.is_finite() && time >= 0.0,
            "invalid JUnit duration for {test}"
        );
        results.push(ResultRecord {
            test,
            outcome,
            duration_ms: (time * 1000.0) as u64,
            classification: match outcome {
                Outcome::Failed => Some("product".into()),
                Outcome::Blocked => Some("infrastructure".into()),
                _ => None,
            },
            reason: detail,
            capabilities_lost: Vec::new(),
            assertions: Vec::new(),
        });
    }
    Ok(results)
}
pub fn counts(expected: usize, results: &[ResultRecord]) -> super::schema::Counts {
    let mut c = super::schema::Counts {
        expected: expected as u64,
        ..Default::default()
    };
    for r in results {
        match r.outcome {
            Outcome::Passed => {
                c.passed += 1;
                c.ran += 1;
            }
            Outcome::Failed => {
                c.failed += 1;
                c.ran += 1;
            }
            Outcome::Blocked => {
                c.blocked += 1;
                c.ran += 1;
            }
            Outcome::Skipped => c.skipped += 1,
            _ => {}
        }
    }
    c
}
