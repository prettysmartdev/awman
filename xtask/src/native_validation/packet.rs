use super::*;
use crate::evidence::schema::Outcome;
use std::collections::BTreeSet;

pub(super) fn files(root: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "symlink in evidence packet");
            if kind.is_dir() {
                walk(root, &entry.path(), out)?;
            } else {
                ensure!(kind.is_file(), "non-file in evidence packet");
                let relative = entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative != "packet.json" {
                    out.insert(relative, sha256_file(&entry.path())?);
                }
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}
fn expected(text: &str) -> Result<BTreeSet<String>> {
    let json: serde_json::Value = serde_json::from_str(text)?;
    let mut out = BTreeSet::new();
    for (_, suite) in json["rust-suites"]
        .as_object()
        .context("missing compiled suite inventory")?
    {
        let id = suite["binary-id"].as_str().context("missing binary ID")?;
        for (name, case) in suite["testcases"]
            .as_object()
            .context("missing compiled tests")?
        {
            if case["filter-match"]["status"] == "matches" {
                ensure!(
                    case["ignored"] == false,
                    "required native test is ignored: {id} {name}"
                );
                ensure!(
                    out.insert(format!("{id} {name}")),
                    "duplicate compiled test"
                );
            }
        }
    }
    ensure!(!out.is_empty(), "required native suite has no tests");
    Ok(out)
}
pub(super) fn check_results(out: &Path, p: &Packet, m: &Manifest) -> Result<()> {
    ensure!(
        p.suites == suites(m),
        "packet omits or changes required native suites"
    );
    for id in &p.suites {
        let want = expected(&fs::read_to_string(out.join(format!("list-{id}.stdout")))?)?;
        let mut actual = BTreeSet::new();
        let evidence = out.join(id);
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(evidence.join("run.json"))?)?;
        ensure!(
            raw["failed_suites"] == serde_json::json!([])
                && raw["runner"] == "nextest"
                && raw["build"] == BUILD,
            "{id}: unsuccessful or wrong runner"
        );
        let inv = raw["invocations"]
            .as_array()
            .context("missing invocations")?;
        ensure!(
            inv.len() == 1 && inv[0]["exit"] == 0 && inv[0]["suites"] == serde_json::json!([id]),
            "{id}: wrong invocation"
        );
        for entry in fs::read_dir(evidence)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "xml") {
                for result in crate::evidence::junit::parse(&fs::read_to_string(path)?)? {
                    ensure!(
                        result.outcome == Outcome::Passed,
                        "{id}: {} did not pass",
                        result.test
                    );
                    ensure!(actual.insert(result.test), "{id}: duplicate result");
                }
            }
        }
        ensure!(
            actual == want,
            "{id}: results differ from independently compiled inventory; missing={:?}, extra={:?}",
            want.difference(&actual).collect::<Vec<_>>(),
            actual.difference(&want).collect::<Vec<_>>()
        );
    }
    Ok(())
}
pub(super) fn verify(root: &Path, out: &Path, m: &Manifest) -> Result<()> {
    let p: Packet = serde_json::from_slice(&fs::read(out.join("packet.json"))?)?;
    ensure!(
        p.schema == 1 && p.verdict == "passed" && p.error.is_none(),
        "packet is not a successful schema-1 run"
    );
    ensure!(p.target == TARGET, "wrong hardware target");
    ensure!(
        p.commit == git(root, &["rev-parse", "HEAD"])?
            && p.tree == git(root, &["rev-parse", "HEAD^{tree}"])?,
        "check out the packet's exact commit before verification"
    );
    ensure!(
        git(root, &["status", "--porcelain"])?.is_empty(),
        "verify from a clean checkout"
    );
    ensure!(
        p.source == source_hashes(root)?,
        "source input digest mismatch"
    );
    ensure!(
        p.files == files(out)?,
        "packet files missing, added or changed"
    );
    ensure!(
        sha256_file(&out.join("candidate/awman"))? == p.artifact_sha256,
        "candidate digest mismatch"
    );
    ensure!(
        sha256_file(&out.join("fixtures/guest-oci-base.tar"))? == p.fixture_sha256,
        "fixture digest mismatch"
    );
    ensure!(
        !p.steps.is_empty() && p.steps.iter().all(|s| s.exit == 0),
        "a command failed"
    );
    let mut names = BTreeSet::new();
    for s in &p.steps {
        ensure!(names.insert(s.name.as_str()), "duplicate command evidence");
    }
    for name in [
        "host-os",
        "host-arch",
        "hypervisor",
        "os-version",
        "rust-version",
        "docker-version",
        "apple-service",
        "sbx-service",
        "docker-context",
        "install-runner",
        "fetch-payload",
        "verify-payload",
        "pull-base",
        "base-provenance",
        "save-base",
        "build-fixture",
        "fetch-previous",
        "sign-awman",
        "sign-awman-guest-driver",
        "sign-awman-guest-net-driver",
        "build-artifact",
        "sign-artifact",
        "verify-artifact",
    ] {
        ensure!(names.contains(name), "missing required command {name}");
        ensure!(
            out.join(format!("{name}.stdout")).is_file()
                && out.join(format!("{name}.stderr")).is_file(),
            "missing logs for {name}"
        );
    }
    for (name, value) in [
        ("host-os", "Darwin"),
        ("host-arch", "arm64"),
        ("hypervisor", "1"),
    ] {
        ensure!(
            fs::read_to_string(out.join(format!("{name}.stdout")))?.trim() == value,
            "invalid native host evidence"
        );
    }
    for id in &p.suites {
        ensure!(
            names.contains(format!("list-{id}").as_str())
                && names.contains(format!("run-{id}").as_str()),
            "missing suite execution evidence"
        );
    }
    let mut original: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("ci/gates.toml"))?)?;
    let fixture = original["fixtures"]["guest-oci-base"]
        .as_table_mut()
        .context("fixture table")?;
    fixture.remove("pending");
    fixture.insert("sha256".into(), p.fixture_sha256.clone().into());
    let config: toml::Value = toml::from_str(&fs::read_to_string(out.join("run-config.toml"))?)?;
    ensure!(
        config == original,
        "run configuration changed more than the prepared fixture pin"
    );
    check_results(out, &p, m)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn listing(ignored: bool) -> String {
        serde_json::json!({"rust-suites":{"guest":{"binary-id":"awman-cli::guest","testcases":{"boot::actual_guest":{"ignored":ignored,"filter-match":{"status":"matches"}}}}}}).to_string()
    }
    #[test]
    fn inventory_requires_real_selected_tests_and_rejects_ignored_cases() {
        assert_eq!(
            expected(&listing(false)).unwrap(),
            BTreeSet::from(["awman-cli::guest boot::actual_guest".into()])
        );
        assert!(expected(&listing(true)).is_err());
        assert!(expected("{\"rust-suites\":{}}").is_err());
        assert!(expected("{}").is_err());
    }
    #[test]
    fn packet_hashes_detect_added_missing_and_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("log"), "passed").unwrap();
        let baseline = files(dir.path()).unwrap();
        fs::write(dir.path().join("log"), "failed").unwrap();
        assert_ne!(files(dir.path()).unwrap(), baseline);
        fs::write(dir.path().join("log"), "passed").unwrap();
        fs::write(dir.path().join("extra"), "extra").unwrap();
        assert_ne!(files(dir.path()).unwrap(), baseline);
        fs::remove_file(dir.path().join("extra")).unwrap();
        fs::remove_file(dir.path().join("log")).unwrap();
        assert_ne!(files(dir.path()).unwrap(), baseline);
    }
}
