//! Every legacy blocking cell has an explicit destination or reviewed deletion.
use crate::manifest::{validate::cells, Manifest};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};
pub fn verify(root: &Path, m: &Manifest) -> Result<()> {
    let baseline: Value = serde_json::from_slice(&std::fs::read(
        root.join("ci/release-coverage-baseline.json"),
    )?)?;
    let ledger: Value = serde_json::from_slice(&std::fs::read(
        root.join("ci/release-coverage-migration.json"),
    )?)?;
    ensure!(
        baseline["schema"] == 1 && ledger["schema"] == 1,
        "unknown migration schema"
    );
    let key = |r: &Value| -> String {
        serde_json::to_string(&serde_json::json!([
            r["old_gate_or_command"],
            r["suite"],
            r["build"],
            r["platform"],
            r["test_filter"]
        ]))
        .expect("JSON")
    };
    let expected: BTreeSet<_> = baseline["cells"]
        .as_array()
        .context("baseline missing cells")?
        .iter()
        .map(key)
        .collect();
    ensure!(!expected.is_empty(), "empty migration baseline");
    let mut seen = BTreeSet::new();
    for row in ledger["cells"]
        .as_array()
        .context("migration missing cells")?
    {
        ensure!(seen.insert(key(row)), "duplicate migration row");
        for field in ["unique_failure_mode", "residual_risk"] {
            ensure!(
                row[field].as_str().is_some_and(|s| !s.is_empty()),
                "migration missing {field}"
            );
        }
        match row["decision"].as_str() {
            Some("DELETE") => ensure!(
                row["new_selector"].is_null(),
                "deleted coverage has destination"
            ),
            Some("KEEP" | "MOVE" | "NARROW" | "SAMPLE") => {
                let dest = &row["new_selector"];
                let gate = dest["gate"].as_str().context("missing migration gate")?;
                if row["decision"] != "NARROW" && row["decision"] != "SAMPLE" {
                    let actual = m
                        .suites
                        .get(dest["suite"].as_str().context("missing migration suite")?)
                        .and_then(|s| s.filter.as_deref());
                    ensure!(
                        row["test_filter"].as_str() == actual,
                        "moved suite test filter changed without a narrowing decision"
                    );
                }
                ensure!(
                    cells(m, gate)?.iter().any(|c| dest["suite"] == c.suite
                        && dest["build"] == c.build
                        && dest["platform"] == c.platform),
                    "invalid migration destination {dest}"
                );
            }
            _ => anyhow::bail!("unknown migration decision"),
        }
    }
    ensure!(
        seen == expected,
        "legacy coverage migration is incomplete or has unreviewed rows"
    );
    Ok(())
}
