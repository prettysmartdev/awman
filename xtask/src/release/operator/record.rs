//! Advisory local records never authorize pipeline gates. Snapshot through a
//! private Git index, so linked worktrees and the user's staged files are safe.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Command};

fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn path() -> Result<PathBuf> {
    Ok(PathBuf::from(git(&[
        "rev-parse",
        "--git-path",
        "awman-release/local-tests.json",
    ])?))
}
fn load() -> Result<Value> {
    let path = path()?;
    if path.exists() {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    } else {
        Ok(json!({"schema":1, "answers":[], "local_tests":"not run"}))
    }
}
fn save(record: &Value) -> Result<()> {
    let path = path()?;
    fs::create_dir_all(path.parent().context("record parent")?)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, serde_json::to_vec(record)?)?;
    fs::rename(temp, path)?;
    Ok(())
}
fn snapshot() -> Result<(String, String, String)> {
    let path = path()?.with_extension(format!("{}.index", std::process::id()));
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    fs::create_dir_all(path.parent().context("index parent")?)?;
    let _cleanup = Remove(path.clone());
    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .env("GIT_INDEX_FILE", &path)
            .args(args)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "cannot snapshot checkout: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8(out.stdout)?.trim().to_owned())
    };
    run(&["read-tree", "HEAD"])?;
    run(&["add", "-A", "--", "."])?;
    let tree = run(&["write-tree"])?;
    // Index intent and executable tool versions are also bound. HEAD itself is
    // deliberately excluded: the release commit must reuse the tested tree.
    let mut hash = Sha256::new();
    hash.update(tree.as_bytes());
    hash.update(git(&["diff", "--cached", "--binary"])?.as_bytes());
    let mut tools = Sha256::new();
    for tool in ["cargo", "rustc"] {
        let out = Command::new(tool).arg("--version").output()?;
        anyhow::ensure!(out.status.success(), "cannot identify {tool}");
        hash.update(&out.stdout);
        tools.update(tool.as_bytes());
        tools.update(out.stdout);
    }
    Ok((
        tree,
        hash.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        tools
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    ))
}
pub fn run(args: &[String]) -> Result<()> {
    let get = |i| {
        args.get(i)
            .map(String::as_str)
            .context("missing internal record argument")
    };
    match get(0)? {
        "--internal-init" => {
            let mut record = load()?;
            record["runner_mode"] = json!(get(1)?);
            if record["answer_version"] != get(2)? {
                record["answers"] = json!([]);
            }
            record["answer_version"] = json!(get(2)?);
            // Answers append across interrupted invocations and remain audit data.
            save(&record)?;
        }
        "--internal-answer" => {
            let mut record = load()?;
            record["answers"]
                .as_array_mut()
                .context("answers array")?
                .push(json!({"prompt":get(1)?, "answer":get(2)?}));
            save(&record)?;
        }
        "--internal-local" => {
            let (tree, fingerprint, tools) = snapshot()?;
            let mut record = load()?;
            let outcome = get(1)?;
            if outcome == "remote" {
                println!(
                    "{}",
                    if tree == git(&["rev-parse", "HEAD^{tree}"])? {
                        "same"
                    } else {
                        "changed"
                    }
                );
            } else if outcome == "query" {
                if record["fingerprint"] == fingerprint
                    && record["tree"] == tree
                    && record["tools"] == tools
                {
                    println!("{}", record["local_tests"].as_str().unwrap_or("not run"));
                } else {
                    println!("not run");
                }
            } else if outcome == "begin" {
                record["begin_tree"] = json!(tree);
                record["begin_fingerprint"] = json!(fingerprint);
                save(&record)?;
            } else {
                anyhow::ensure!(
                    record["begin_tree"] == tree && record["begin_fingerprint"] == fingerprint,
                    "source changed while local tests were running; retry Tests"
                );
                anyhow::ensure!(
                    ["passed", "skipped by operator", "not run"].contains(&outcome),
                    "invalid local outcome"
                );
                record["tree"] = json!(tree);
                record["fingerprint"] = json!(fingerprint);
                record["tools"] = json!(tools);
                record["local_tests"] = json!(outcome);
                save(&record)?;
            }
        }
        "--internal-commit" => {
            let mut record = load()?;
            let (tree, fingerprint, tools) = snapshot()?;
            if record["tree"] == tree && record["tools"] == tools {
                record["fingerprint"] = json!(fingerprint);
                save(&record)?;
            }
        }
        "--internal-payload" | "--internal-journal-payload" => {
            let mut record = load()?;
            let (tree, _, tools) = snapshot()?;
            // Journals may describe a candidate from an earlier checkout. Bind
            // the advisory outcome to that candidate's tree, never merely HEAD.
            let record_only = get(0)? == "--internal-journal-payload";
            let candidate_tree = git(&["rev-parse", &format!("{}^{{tree}}", get(1)?)]);
            // A shallow/new checkout may not yet contain a resumed candidate.
            // Its answers are still recordable, with no claimed local pass.
            let commit_tree = if record_only {
                candidate_tree.ok()
            } else {
                Some(candidate_tree?)
            };
            if record["tree"] != tree
                || commit_tree.as_deref() != Some(tree.as_str())
                || record["tools"] != tools
            {
                record["tested_checkout_outcome"] = record["local_tests"].clone();
                record["local_tests"] = json!("not run");
                record["reason"] = json!("tested checkout or tools differ from release inputs");
            }
            record["commit"] = json!(get(1)?);
            record["record_only"] = json!(record_only);
            // Stable across machines and dispatch retries; GitHub serializes by
            // version and the pipeline resolves this ID against full input pins.
            record["request_id"] = json!(crate::evidence::digest::sha256(
                format!("{}:{}:{}", get(2)?, get(3)?, get(1)?).as_bytes()
            ));
            // This projection is candidate-specific. In particular a journal
            // for an old candidate must not erase a valid newer checkout pass.
            println!("{}", serde_json::to_string(&record)?);
        }
        "--internal-lock-version" => {
            let lock: toml::Value = toml::from_str(&fs::read_to_string("Cargo.lock")?)?;
            let packages = lock["package"].as_array().context("lock packages")?;
            let ready = packages.iter().filter(|p| own_version(p)).all(|p| {
                p.get("version").and_then(toml::Value::as_str) == Some(get(1).unwrap_or(""))
            });
            println!("{}", if ready { "ready" } else { "update" });
        }
        "--internal-lock" => {
            // Version bump must not silently update dependency pins. Compare
            // complete package records except versions of workspace packages.
            let before: toml::Value = toml::from_str(&fs::read_to_string(get(1)?)?)?;
            let after: toml::Value = toml::from_str(&fs::read_to_string("Cargo.lock")?)?;
            fn pins(mut value: toml::Value) -> toml::Value {
                if let Some(packages) = value.get_mut("package").and_then(toml::Value::as_array_mut)
                {
                    for package in packages {
                        if own_version(package) {
                            if let Some(table) = package.as_table_mut() {
                                table.remove("version");
                            }
                        }
                    }
                }
                value
            }
            anyhow::ensure!(
                pins(before) == pins(after),
                "cargo update changed dependency pins; restore Cargo.lock and retry"
            );
        }
        other => bail!("unknown operator internal command {other}"),
    }
    Ok(())
}

fn own_version(package: &toml::Value) -> bool {
    package.get("source").is_none()
        && matches!(
            package.get("name").and_then(toml::Value::as_str),
            Some("awman" | "awman-cli" | "awman-wsp" | "awman-host-shims")
        )
}
