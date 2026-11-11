//! Authenticate Actions artifacts, then validate the complete selected execution.
//! Local JSON cannot opt into this transport. Release validation still uses its
//! stricter artifact/signing/critical-scenario policy in release::state.
use super::{digest, junit, schema::*};
use crate::{
    gates::{
        plan::{self, Plan},
        render::workflows,
    },
    manifest::Manifest,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::{Command, Stdio},
};

fn output(program: &str, args: &[&str], root: Option<&Path>) -> Result<Vec<u8>> {
    let mut c = Command::new(program);
    c.args(args);
    if let Some(root) = root {
        c.current_dir(root);
    }
    let o = c.output()?;
    ensure!(
        o.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    Ok(o.stdout)
}
pub fn source(root: &Path) -> Result<Value> {
    let git = |arg| -> Result<String> {
        Ok(
            String::from_utf8(output("git", &["rev-parse", arg], Some(root))?)?
                .trim()
                .into(),
        )
    };
    let mut inputs = BTreeMap::new();
    for name in [
        "ci/gates.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".config/nextest.toml",
    ] {
        inputs.insert(name, digest::sha256_file(&root.join(name))?);
    }
    let clean = Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--"])
        .current_dir(root)
        .status()?
        .success();
    Ok(json!({"commit":git("HEAD")?, "tree":git("HEAD^{tree}")?, "clean":clean, "inputs":inputs}))
}
fn api(path: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&output(
        "gh",
        &["api", path],
        None,
    )?)?)
}
fn pages(path: &str, field: &str) -> Result<Vec<Value>> {
    let data: Vec<Value> = serde_json::from_slice(&output(
        "gh",
        &["api", "--paginate", "--slurp", path],
        None,
    )?)?;
    let mut all = Vec::new();
    for page in data {
        all.extend(
            page[field]
                .as_array()
                .context("malformed service page")?
                .iter()
                .cloned(),
        );
    }
    Ok(all)
}
fn environment(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required; aggregation runs in Actions"))
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().with_context(|| format!("missing {key}"))
}
fn number(v: &Value, key: &str) -> Result<u64> {
    v[key].as_u64().with_context(|| format!("missing {key}"))
}

/// Service digest authenticates the compressed bytes, then extraction rejects
/// traversal, links and non-files. Nothing from an evidence archive is executed.
fn download(repo: &str, artifact: &Value, dir: &Path) -> Result<()> {
    ensure!(artifact["expired"] == false, "artifact expired");
    let id = number(artifact, "id")?;
    let expected = text(artifact, "digest")?
        .strip_prefix("sha256:")
        .context("service must supply artifact SHA-256")?;
    ensure!(digest::is_sha256(expected), "invalid service digest");
    fs::create_dir_all(dir)?;
    let zip = dir.with_extension("zip");
    let status = Command::new("gh")
        .args(["api", &format!("repos/{repo}/actions/artifacts/{id}/zip")])
        .stdout(fs::File::create(&zip)?)
        .status()?;
    ensure!(status.success(), "download artifact {id} failed");
    ensure!(
        digest::sha256_file(&zip)? == expected,
        "artifact {id}: service digest mismatch"
    );
    let script = r#"import pathlib, stat, sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as z:
    seen = set()
    for i in z.infolist():
        p = pathlib.PurePosixPath(i.filename)
        mode = i.external_attr >> 16
        assert not p.is_absolute() and '..' not in p.parts and '\\' not in i.filename
        assert i.filename not in seen
        seen.add(i.filename)
        assert stat.S_IFMT(mode) in (0, stat.S_IFREG, stat.S_IFDIR)
    z.extractall(sys.argv[2])
"#;
    let status = Command::new("python3")
        .args(["-c", script])
        .arg(&zip)
        .arg(dir)
        .stdin(Stdio::null())
        .status()?;
    ensure!(status.success(), "unsafe or invalid evidence ZIP");
    fs::remove_file(zip)?;
    Ok(())
}
fn artifact<'a>(all: &'a [Value], name: &str) -> Result<&'a Value> {
    let matching = all.iter().filter(|v| v["name"] == name).collect::<Vec<_>>();
    ensure!(
        matching.len() == 1,
        "required artifact {name}: expected one, found {}",
        matching.len()
    );
    Ok(matching[0])
}
fn job_passed(jobs: &[Value], name: &str, count: usize) -> Result<()> {
    let matching = jobs
        .iter()
        .filter(|j| {
            j["name"].as_str().is_some_and(|n| {
                let n = n.rsplit(" / ").next().unwrap_or(n);
                n == name || n.starts_with(&format!("{name} ("))
            })
        })
        .collect::<Vec<_>>();
    ensure!(
        matching.len() == count
            && matching
                .iter()
                .all(|j| j["conclusion"] == "success" && j["status"] == "completed"),
        "required job {name}: missing, renamed, skipped or unsuccessful"
    );
    Ok(())
}
fn expected(v: &Value) -> Result<BTreeSet<String>> {
    let mut tests = BTreeSet::new();
    for suite in v["rust-suites"]
        .as_object()
        .context("missing compiled inventory")?
        .values()
    {
        let id = text(suite, "binary-id")?;
        for (name, case) in suite["testcases"].as_object().context("missing tests")? {
            if case["filter-match"]["status"] == "matches" {
                ensure!(
                    case["ignored"] == false,
                    "required test is ignored: {id} {name}"
                );
                ensure!(
                    tests.insert(format!("{id} {name}")),
                    "duplicate inventory ID"
                );
            }
        }
    }
    ensure!(!tests.is_empty(), "required test invocation is empty");
    Ok(tests)
}
fn file_in(dir: &Path, name: &str) -> Result<std::path::PathBuf> {
    ensure!(
        !name.is_empty()
            && Path::new(name).components().count() == 1
            && name != "."
            && name != "..",
        "invalid evidence filename"
    );
    Ok(dir.join(name))
}
/// Pure checks used after service authentication. Never supplies its own trust bit.
fn check_run(
    dir: &Path,
    suites: &BTreeSet<String>,
    build: &str,
    shard: Option<&str>,
    source: &Value,
    producer: &Value,
    m: &Manifest,
) -> Result<BTreeMap<String, Counts>> {
    let raw: Value = serde_json::from_slice(&fs::read(dir.join("run.json"))?)?;
    ensure!(
        &raw["source"] == source && source["clean"] == true,
        "stale or dirty source evidence"
    );
    ensure!(&raw["producer"] == producer, "wrong producer/run attempt");
    ensure!(
        raw["runner"] == "nextest" && raw["build"] == build && raw["shard"] == json!(shard),
        "wrong build/runner/shard"
    );
    ensure!(raw["failed_suites"] == json!([]), "run has failed suites");
    let mut seen = BTreeSet::new();
    let mut want = BTreeSet::new();
    let mut by_suite = BTreeMap::new();
    for inv in raw["invocations"]
        .as_array()
        .context("missing invocations")?
    {
        ensure!(inv["exit"] == 0, "invocation failed");
        let ids = inv["suites"].as_array().context("missing suite IDs")?;
        ensure!(
            ids.len() == 1,
            "each suite must have its own invocation and inventory"
        );
        let mut is_nextest = false;
        for id in ids {
            let id = id.as_str().context("suite ID")?;
            ensure!(
                suites.contains(id) && seen.insert(id.to_string()),
                "extra or duplicated suite {id}"
            );
            is_nextest |= m.suites[id].kind == "nextest";
        }
        if is_nextest {
            let file = file_in(dir, text(inv, "expected")?)?;
            let selected = expected(&serde_json::from_slice(&fs::read(file)?)?)?;
            by_suite.insert(ids[0].as_str().unwrap().to_string(), selected.len());
            for test in selected {
                ensure!(want.insert(test), "test selected twice in one job");
            }
        } else {
            by_suite.insert(ids[0].as_str().unwrap().to_string(), 0);
        }
    }
    ensure!(&seen == suites, "missing selected suites");
    let mut got = BTreeSet::new();
    for file in fs::read_dir(dir)? {
        let path = file?.path();
        if path.extension().is_some_and(|s| s == "xml") {
            for r in junit::parse(&fs::read_to_string(path)?)? {
                ensure!(
                    r.outcome == Outcome::Passed,
                    "required test {} did not pass",
                    r.test
                );
                ensure!(got.insert(r.test.clone()), "duplicated test {}", r.test);
            }
        }
    }
    ensure!(
        got == want,
        "JUnit disagrees with compiled inventory (missing: {:?}; extra: {:?})",
        want.difference(&got).collect::<Vec<_>>(),
        got.difference(&want).collect::<Vec<_>>()
    );
    Ok(by_suite
        .into_iter()
        .map(|(id, count)| {
            (
                id,
                Counts {
                    expected: count as u64,
                    ran: count as u64,
                    passed: count as u64,
                    ..Default::default()
                },
            )
        })
        .collect())
}

pub fn aggregate(root: &Path, m: &Manifest, p: &Plan) -> Result<GateEvidence> {
    ensure!(
        environment("GITHUB_ACTIONS")? == "true",
        "authenticated Actions context required"
    );
    let repo = environment("GITHUB_REPOSITORY")?;
    ensure!(
        repo.split('/').count() == 2
            && repo
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/-_.".contains(&c)),
        "invalid repository"
    );
    let run_id: u64 = environment("GITHUB_RUN_ID")?.parse()?;
    let attempt: u64 = environment("GITHUB_RUN_ATTEMPT")?.parse()?;
    let run = api(&format!("repos/{repo}/actions/runs/{run_id}"))?;
    ensure!(number(&run, "run_attempt")? == attempt, "stale run attempt");
    let event: Value = serde_json::from_slice(&fs::read(environment("GITHUB_EVENT_PATH")?)?)?;
    let event_name = environment("GITHUB_EVENT_NAME")?;
    ensure!(run["event"] == event_name, "event differs from service");
    let current = source(root)?;
    ensure!(
        current["clean"] == true && current["commit"] == environment("GITHUB_SHA")?,
        "checkout differs from triggering source"
    );
    // PR API head_sha describes the contributor head; GITHUB_SHA is the tested
    // merge commit. Both are checked against the provider event envelope.
    let service_head = if event_name == "pull_request" {
        &event["pull_request"]["head"]["sha"]
    } else {
        &current["commit"]
    };
    ensure!(&run["head_sha"] == service_head, "service head mismatch");
    let mut request = p.request.clone();
    request.repository_id = number(&run["repository"], "id")?.to_string();
    request.event = event_name.clone();
    request.head_sha = text(&current, "commit")?.into();
    let base = match event_name.as_str() {
        "pull_request" => event["pull_request"]["base"]["sha"].as_str(),
        "merge_group" => event["merge_group"]["base_sha"].as_str(),
        "push" => event["before"].as_str(),
        _ => None,
    };
    request.base_sha = base
        .filter(|s| !s.chars().all(|c| c == '0'))
        .map(str::to_string);
    request.paths = match &request.base_sha {
        Some(base) => plan::changed_paths(root, base, &request.head_sha)?,
        None => vec![],
    };
    request.diff_complete = request.base_sha.is_some();
    // This controller never turns uploader assertions into hardware admission.
    request.admitted_platforms.clear();
    let today = chrono::Utc::now().date_naive();
    ensure!(
        plan::create(m, &p.gate, request, today)? == *p,
        "plan differs from independently reconstructed event and diff"
    );
    let jobs = pages(
        &format!("repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"),
        "jobs",
    )?;
    let artifacts = pages(
        &format!("repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"),
        "artifacts",
    )?;
    let mode = std::env::var("GATE_MODE").unwrap_or_default();
    let prefix = format!("evidence-{}-{mode}-", p.gate);
    let scratch = tempfile::tempdir()?;
    job_passed(&jobs, "Plan", 1)?;
    let plan_name = format!("plan-{}-{mode}-{}", p.gate, environment("GITHUB_SHA")?);
    download(
        &repo,
        artifact(&artifacts, &plan_name)?,
        &scratch.path().join("plan"),
    )?;
    let uploaded: Plan = serde_json::from_slice(&fs::read(scratch.path().join("plan/plan.json"))?)?;
    ensure!(uploaded == *p, "plan artifact differs");
    let mut report = GateEvidence {
        schema: 1,
        kind: "gate".into(),
        gate: p.gate.clone(),
        commit: p.request.head_sha.clone(),
        manifest_version: m.version().into(),
        plan_sha256: p.digest()?,
        verdict: Outcome::Passed,
        cells: vec![],
        missing: vec![],
        errors: crate::manifest::validate::pins_for_gate(m, &p.gate),
        release_critical: vec![],
        quarantine: p.quarantine.clone(),
        overrides_applied: vec![],
        queue_ms: 0,
        execution_ms: 0,
        p95_budget_minutes: m.gates[&p.gate].budget_minutes,
        generated_at: chrono::Utc::now(),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(24)),
    };
    for b in &p.builds {
        if let Err(e) = job_passed(&jobs, &format!("Build {}", b.id), 1) {
            report.errors.push(e.to_string());
        }
    }
    let outputs = crate::ci::plan_outputs::outputs(m, p)?;
    let matrices = outputs["shards"]
        .as_object()
        .context("missing shard matrices")?;
    let mut checked: BTreeMap<String, (String, Counts)> = BTreeMap::new();
    for (job, matrix) in matrices {
        let entries = matrix.as_array().context("invalid matrix")?;
        let t = p
            .tests
            .iter()
            .find(|t| workflows::test_job_id(m, &t.tier, &t.platform, &t.build) == *job)
            .context("unplanned job")?;
        let job_result = job_passed(
            &jobs,
            &format!("Test {} on {}", t.tier, t.platform),
            entries.len(),
        );
        if let Err(e) = job_result {
            report.errors.push(e.to_string());
        }
        for (index, entry) in entries.iter().enumerate() {
            let ids: BTreeSet<String> = text(entry, "suites")?
                .split(',')
                .map(str::to_string)
                .collect();
            let name = format!("{prefix}{job}-m{index}");
            let dir = scratch.path().join(&name);
            let check = (|| -> Result<(String, BTreeMap<String, Counts>)> {
                let a = artifact(&artifacts, &name)?;
                download(&repo, a, &dir)?;
                let producer =
                    json!({"run_id":run_id.to_string(), "attempt":attempt.to_string(), "job":job});
                let shard = format!("{}/{}", number(entry, "i")?, number(entry, "n")?);
                let counts = check_run(&dir, &ids, &t.build, Some(&shard), &current, &producer, m)?;
                Ok((text(a, "digest")?.into(), counts))
            })();
            match check {
                Ok((digest, counts)) => {
                    for id in ids {
                        checked.insert(
                            format!("{id}/{}:{index}", t.build),
                            (digest.clone(), counts[&id].clone()),
                        );
                    }
                }
                Err(e) => report.errors.push(format!("{name}: {e:#}")),
            }
        }
    }
    for (job, title) in [
        ("lint", "Lint"),
        ("web", "Web app, edge Worker and browser acceptance"),
    ] {
        let selected: Vec<_> = p
            .cells
            .iter()
            .filter(|c| {
                c.selected
                    && m.suites.get(&c.cell.suite).is_some_and(|s| {
                        s.kind != "nextest"
                            && if job == "web" {
                                c.cell.suite == "web"
                            } else {
                                c.cell.suite != "web"
                            }
                    })
            })
            .collect();
        if selected.is_empty() {
            continue;
        }
        let ids = selected.iter().map(|c| c.cell.suite.clone()).collect();
        let build = &selected[0].cell.build;
        let name = format!("{prefix}{job}");
        let dir = scratch.path().join(&name);
        let check = (|| -> Result<(String, BTreeMap<String, Counts>)> {
            job_passed(&jobs, title, 1)?;
            let a = artifact(&artifacts, &name)?;
            download(&repo, a, &dir)?;
            let producer =
                json!({"run_id":run_id.to_string(), "attempt":attempt.to_string(), "job":job});
            let counts = check_run(&dir, &ids, build, None, &current, &producer, m)?;
            Ok((text(a, "digest")?.into(), counts))
        })();
        match check {
            Ok((digest, counts)) => {
                for c in selected {
                    checked.insert(
                        format!("{}/{}:0", c.cell.suite, c.cell.build),
                        (digest.clone(), counts[&c.cell.suite].clone()),
                    );
                }
            }
            Err(e) => report.errors.push(format!("{name}: {e:#}")),
        }
    }
    for c in &p.cells {
        if !c.selected {
            report.cells.push(GateCell {
                cell: c.cell.id.clone(),
                outcome: Outcome::NotSelected,
                evidence_sha256: vec![],
                counts: Counts::default(),
            });
            continue;
        }
        // Build-only cells are certified by their authenticated build job above.
        let is_test = m.suites.contains_key(&c.cell.suite);
        let matches = checked
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{}/{}:", c.cell.suite, c.cell.build)))
            .map(|(_, v)| v)
            .collect::<Vec<_>>();
        if is_test && matches.len() != c.shards as usize {
            report.missing.push(c.cell.id.clone());
        }
        report.cells.push(GateCell {
            cell: c.cell.id.clone(),
            outcome: if is_test && matches.len() != c.shards as usize {
                Outcome::Failed
            } else {
                Outcome::Passed
            },
            evidence_sha256: matches.iter().map(|(d, _)| d.clone()).collect(),
            counts: matches.iter().fold(Counts::default(), |mut total, (_, c)| {
                total.expected += c.expected;
                total.ran += c.ran;
                total.passed += c.passed;
                total.failed += c.failed;
                total.skipped += c.skipped;
                total.blocked += c.blocked;
                total.quarantined_failed += c.quarantined_failed;
                total
            }),
        });
    }
    if !report.errors.is_empty() || !report.missing.is_empty() {
        report.verdict = Outcome::Failed;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(dir: &Path) -> (Value, Value, Manifest, BTreeSet<String>) {
        let source = json!({"commit":"source", "clean":true});
        let producer = json!({"run_id":"42", "attempt":"2", "job":"test-unit-linux-x64"});
        let suites = BTreeSet::from(["unit-test-support".to_string()]);
        let raw = json!({"source":source, "producer":producer, "runner":"nextest", "build":"linux-x64-default", "shard":"1/1", "failed_suites":[], "invocations":[{"exit":0,"suites":["unit-test-support"],"expected":"expected.json"}]});
        fs::write(dir.join("run.json"), serde_json::to_vec(&raw).unwrap()).unwrap();
        fs::write(dir.join("expected.json"), serde_json::to_vec(&json!({"rust-suites":{"support":{"binary-id":"awman-test-support","testcases":{"isolation":{"ignored":false,"filter-match":{"status":"matches"}}}}}})).unwrap()).unwrap();
        fs::write(dir.join("junit.xml"), "<testsuite><testcase classname=\"awman-test-support\" name=\"isolation\"/></testsuite>").unwrap();
        (
            source,
            producer,
            crate::manifest::load(&crate::gates::root()).unwrap(),
            suites,
        )
    }
    #[test]
    fn raw_results_require_complete_passing_inventory_and_current_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let (source, producer, m, suites) = setup(dir.path());
        let check = || {
            check_run(
                dir.path(),
                &suites,
                "linux-x64-default",
                Some("1/1"),
                &source,
                &producer,
                &m,
            )
        };
        assert_eq!(check().unwrap()["unit-test-support"].passed, 1);
        for xml in ["<testsuite/>", "<testsuite><testcase classname=\"awman-test-support\" name=\"isolation\"><skipped/></testcase></testsuite>", "<testsuite><testcase classname=\"awman-test-support\" name=\"isolation\"><failure/></testcase></testsuite>", "<testsuite><testcase classname=\"awman-test-support\" name=\"wrong\"/></testsuite>"] {
            fs::write(dir.path().join("junit.xml"), xml).unwrap();
            assert!(check().is_err(), "accepted {xml}");
        }
        setup(dir.path());
        for (key, bad) in [
            ("source", json!({"commit":"other", "clean":true})),
            (
                "producer",
                json!({"run_id":"42", "attempt":"1", "job":"test-unit-linux-x64"}),
            ),
            ("shard", json!("2/2")),
            ("failed_suites", json!(["unit-test-support"])),
            ("invocations", json!([])),
        ] {
            setup(dir.path());
            let file = dir.path().join("run.json");
            let mut raw: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
            raw[key] = bad;
            fs::write(file, serde_json::to_vec(&raw).unwrap()).unwrap();
            assert!(check().is_err(), "accepted changed {key}");
        }
        setup(dir.path());
        fs::remove_file(dir.path().join("expected.json")).unwrap();
        assert!(check().is_err());
    }
    #[test]
    fn job_authentication_rejects_skips_renames_and_missing_matrix_members() {
        let good = json!({"name":"Test unit on linux-x64 (1, 1)","conclusion":"success","status":"completed"});
        assert!(job_passed(std::slice::from_ref(&good), "Test unit on linux-x64", 1).is_ok());
        assert!(job_passed(std::slice::from_ref(&good), "Test unit on linux-x64", 2).is_err());
        assert!(job_passed(std::slice::from_ref(&good), "Test unit on windows-x64", 1).is_err());
        let mut skipped = good;
        skipped["conclusion"] = json!("skipped");
        assert!(job_passed(&[skipped], "Test unit on linux-x64", 1).is_err());
    }
}
