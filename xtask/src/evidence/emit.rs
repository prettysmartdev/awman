use super::{aggregate, digest, junit, schema::*};
use anyhow::{ensure, Result};
use std::path::Path;
/// Assertions/capability losses are runtime records; never inferred from a JUnit PASS.
pub fn merge_records(results: &mut [ResultRecord], records: &[ResultRecord]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for record in records {
        ensure!(
            seen.insert(&record.test),
            "duplicate runtime record {}",
            record.test
        );
        let result = results
            .iter_mut()
            .find(|r| r.test == record.test)
            .ok_or_else(|| anyhow::anyhow!("unknown runtime record {}", record.test))?;
        ensure!(
            result.outcome == record.outcome || !record.capabilities_lost.is_empty(),
            "contradictory JUnit/runtime outcome for {}",
            record.test
        );
        result.assertions = record.assertions.clone();
        result.capabilities_lost = record.capabilities_lost.clone();
        if !result.capabilities_lost.is_empty() {
            result.outcome = Outcome::Blocked;
            result.classification = Some("infrastructure".into());
        }
    }
    Ok(())
}
pub fn finish(
    mut e: Evidence,
    expected: &ExpectedSet,
    results: Vec<ResultRecord>,
) -> Result<Evidence> {
    ensure!(
        expected.cell == e.cell
            && expected.build == e.build
            && expected.manifest_version == e.manifest_version,
        "expected set belongs to another cell/build/manifest"
    );
    e.expected_set_sha256 = digest::input_digest(expected)?;
    e.archive_sha256 = expected.archive_sha256.clone();
    e.input_digest = digest::input_digest(&e.inputs)?;
    aggregate::partitions(expected, e.shard.of)?;
    let mut shard_expected = expected.clone();
    shard_expected.tests = expected
        .partitions
        .get(&e.shard.index)
        .ok_or_else(|| anyhow::anyhow!("unknown shard {}", e.shard.index))?
        .clone();
    e.counts = junit::counts(shard_expected.tests.len(), &results);
    e.results = results;
    let valid = aggregate::compare(&shard_expected, &e.results);
    let phases_ok = ["setup", "run", "sweep", "upload"]
        .iter()
        .all(|p| e.phases.get(*p) == Some(&Outcome::Passed));
    e.outcome = if valid.is_ok() && phases_ok && e.command_exit == 0 {
        Outcome::Passed
    } else if e.results.iter().any(|r| r.outcome == Outcome::Failed) {
        Outcome::Failed
    } else {
        Outcome::Blocked
    };
    e.classification = match e.outcome {
        Outcome::Passed => None,
        Outcome::Failed => Some("product".into()),
        _ => Some("infrastructure".into()),
    };
    if let Err(error) = valid {
        eprintln!("{}: {error:#}", e.cell);
    }
    Ok(e)
}
pub fn write(dir: &Path, e: &Evidence) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("evidence.json");
    // Atomic replacement within one filesystem, no half-written passing record.
    let pending = dir.join(format!(".evidence-{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(e)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(pending, path)?;
    Ok(())
}
pub fn read_directory(dir: &Path) -> Result<Vec<Evidence>> {
    let mut results = Vec::new();
    if !dir.exists() {
        return Ok(results);
    }
    for entry in std::fs::read_dir(dir)? {
        let e = entry?;
        if e.file_type()?.is_dir() {
            results.extend(read_directory(&e.path())?);
        } else if e.file_type()?.is_file() && e.file_name() == "evidence.json" {
            results.push(serde_json::from_slice(&std::fs::read(e.path())?)?);
        }
    }
    Ok(results)
}
