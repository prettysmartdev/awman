//! Release smoke of the Docker Sandboxes runtime (native check
//! `native.macos-arm64-sbx`, suite `artifact-sbx-smoke`): exactly two cases
//! through the exact release executable, each `awman ready` plus one safe,
//! bounded headless task with a sandbox-scoped test credential, workspace
//! scope, and owned cleanup.
//!
//! - Claude: Docker's published workload plus awman's v3 mixin.
//! - OpenCode 1: an awman-owned v3 workload (the custom-workload selector
//!   and the entrypoint gate).
//!
//! The release kit runner reserves each case's private workspace and its
//! production sandbox name in `$AWMAN_TEST_EVIDENCE_DIR/sbx-owned.json`
//! before any test runs; a case uses exactly that workspace and removes
//! exactly that sandbox. Missing CLI, identity, network or reservation is
//! `BLOCKED`. While the release switch of the runtime is off, these cases
//! fail: live qualification must precede the switch flip.
//!
//! This binary does not link the product library (hygiene rule H9).

use crate::support;
use awman_test_support::capability::{DockerSbx, PublicNetwork, SbxTestIdentity};
use awman_test_support::TestSandbox;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A fixed prompt asking for a one-word reply and no tool use.
const PROMPT: &str = "Reply with exactly one word: ready. Do not use any tools.";
const CLI_DEADLINE: Duration = Duration::from_secs(120);
const TASK_DEADLINE: Duration = Duration::from_secs(420);

struct Case {
    test: &'static str,
    agent: &'static str,
    disposition: &'static str,
    kit_handle: &'static str,
}

/// Removes the reserved sandbox on every path out of the case.
struct Owned<'a> {
    s: &'a TestSandbox,
    cli: PathBuf,
    sandbox: String,
    removed: bool,
}

impl Owned<'_> {
    fn remove(&mut self) -> bool {
        let removal = self
            .s
            .command(&self.cli)
            .args(["rm", "--force", self.sandbox.as_str()])
            .deadline(CLI_DEADLINE)
            .run();
        let listing = self
            .s
            .command(&self.cli)
            .args(["--no-input", "ls", "--json"])
            .deadline(CLI_DEADLINE)
            .run();
        self.removed = awman_test_support::sbx::removal_verified(
            removal.exit.success(),
            listing.exit.success(),
            &listing.stdout,
            &self.sandbox,
        );
        self.removed
    }
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        if !self.removed {
            self.remove();
        }
    }
}

/// The first `MAJOR.MINOR.PATCH` token of `sbx --version`.
fn cli_version(raw: &str) -> Option<String> {
    raw.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find(|part| {
            let fields: Vec<_> = part.split('.').collect();
            fields.len() == 3 && fields.iter().all(|f| f.parse::<u64>().is_ok())
        })
        .map(str::to_string)
}

/// The runner's reservation for `agent`: (workspace, sandbox name).
fn reservation(evidence: &Path, agent: &str) -> (PathBuf, String) {
    let path = evidence.join("sbx-owned.json");
    let text = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "BLOCKED[sbx-test-identity]: the release kit runner reserved no SBX workspace ({}: {e})",
            path.display()
        )
    });
    let owned: Value = serde_json::from_slice(&text).expect("sbx-owned.json");
    let case = owned["cases"]
        .as_array()
        .and_then(|cases| cases.iter().find(|c| c["agent"] == agent))
        .unwrap_or_else(|| panic!("sbx-owned.json reserves nothing for {agent}"));
    (
        PathBuf::from(case["workspace"].as_str().expect("workspace")),
        case["sandbox"].as_str().expect("sandbox").to_string(),
    )
}

fn smoke(case: Case) {
    let mut s = support::sandbox(case.test);
    let sbx = s.require::<DockerSbx>();
    let identity = s.require::<SbxTestIdentity>();
    let _network = s.require::<PublicNetwork>();
    let evidence = awman_test_support::runtime_env::get()
        .evidence_dir
        .clone()
        .unwrap_or_else(|| {
            panic!("BLOCKED[sbx-test-identity]: AWMAN_TEST_EVIDENCE_DIR is not set")
        });
    let (workspace, sandbox) = reservation(&evidence, case.agent);
    let a = support::artifact(&mut s);
    s.grant(&sbx);
    s.grant(&identity);
    let key = identity.anthropic_key().to_string();
    s.redact(&key);
    s.note("sbx_identity", &identity.identity);

    let version = s
        .command(&sbx.cli)
        .arg("--version")
        .deadline(CLI_DEADLINE)
        .run();
    let sbx_cli = cli_version(&version.stdout_text()).unwrap_or_else(|| {
        panic!(
            "BLOCKED[docker-sbx]: unreadable sbx --version: {:?}",
            version.stdout_text()
        )
    });
    s.note("sbx_cli", &sbx_cli);
    std::fs::create_dir_all(s.awman_home()).unwrap();
    std::fs::write(
        s.awman_home().join("config.json"),
        serde_json::to_vec(&json!({
            "runtime": {"kind": "sbx"},
            "agents": {"auth": {"refresh": {"enabled": false}}}
        }))
        .unwrap(),
    )
    .unwrap();

    // A fresh Git repository in the reserved workspace.
    assert!(
        std::fs::read_dir(&workspace).unwrap().next().is_none(),
        "reserved workspace {} is not empty",
        workspace.display()
    );
    std::fs::write(workspace.join("README.md"), "awman sbx release smoke\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "README.md"][..],
        &[
            "-c",
            "user.name=awman smoke",
            "-c",
            "user.email=smoke@awman.invalid",
            "commit",
            "-q",
            "-m",
            "smoke",
        ][..],
    ] {
        s.command("git")
            .args(args.iter().copied())
            .current_dir(&workspace)
            .run()
            .assert_success();
    }
    let mut owned = Owned {
        s: &s,
        cli: sbx.cli.clone(),
        sandbox: sandbox.clone(),
        removed: false,
    };

    // 1. ready through the exact artifact.
    let ready = s
        .command(&a.exe)
        .label("exact artifact ready")
        .args(["ready", "--agent", case.agent])
        .current_dir(&workspace)
        .deadline(TASK_DEADLINE)
        .run();
    ready.assert_success();

    // 2. One bounded headless task with the sandbox-scoped test credential.
    let task = s
        .command(&a.exe)
        .label("exact artifact headless task")
        .args([
            "exec",
            "prompt",
            "--agent",
            case.agent,
            "--headless",
            "--overlay",
            "env(ANTHROPIC_API_KEY)",
            PROMPT,
        ])
        .env("ANTHROPIC_API_KEY", &key)
        .current_dir(&workspace)
        .deadline(TASK_DEADLINE)
        .run();
    task.assert_success();
    let output_bytes = task.stdout_text().trim().len();
    assert!(output_bytes > 0, "the agent produced no output");

    // 3. Scope: the workspace is shared at its host path; its parent is not.
    std::fs::write(workspace.join("smoke-sentinel"), "inside\n").unwrap();
    let outside = workspace.parent().unwrap().join("outside-sentinel");
    std::fs::write(&outside, "outside\n").unwrap();
    let inside = s
        .command(&sbx.cli)
        .args(["--no-input", "exec", sandbox.as_str(), "cat"])
        .arg(workspace.join("smoke-sentinel"))
        .deadline(CLI_DEADLINE)
        .run();
    inside.assert_success();
    assert_eq!(inside.stdout_text().trim(), "inside");
    let escaped = s
        .command(&sbx.cli)
        .args(["--no-input", "exec", sandbox.as_str(), "cat"])
        .arg(&outside)
        .deadline(CLI_DEADLINE)
        .run();
    assert!(
        !escaped.exit.success() || !escaped.stdout_text().contains("outside"),
        "the workspace parent is reachable inside the sandbox"
    );

    // Identity copied from awman's host record of the sandbox.
    let record_path = s
        .awman_home()
        .join("sbx/sandboxes")
        .join(format!("{sandbox}.json"));
    let record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap_or_else(|e| {
        panic!(
            "awman wrote no identity record {}: {e}",
            record_path.display()
        )
    }))
    .unwrap();
    assert_eq!(record["agent"], case.agent);
    let workload = if case.disposition == "owned-workload" {
        Value::Null
    } else {
        json!(case.agent)
    };
    assert_eq!(record["workload"], workload);
    assert_eq!(record["disposition"], case.disposition);
    assert_eq!(record["kit_handle"], case.kit_handle);

    // 5. Owned cleanup, verified.
    let removed = owned.remove();
    assert!(removed, "owned sandbox {sandbox} survived removal");
    drop(owned);

    // 4. Evidence for the seal: identities only, never a key or the output.
    let file = evidence.join(format!("sbx-smoke.{}.json", case.agent));
    let value = json!({
        "schema": 1,
        "test": format!("awman-cli::artifact {}", case.test),
        "agent": record["agent"],
        "disposition": record["disposition"],
        "workload": record["workload"],
        "kit_handle": record["kit_handle"],
        "kit_digest": record["kit_digest"],
        "sandbox": sandbox,
        "sbx_cli": sbx_cli,
        "inspect_sha256": record["inspect_sha256"],
        "kits": record["kits"],
        "ready_exit": if ready.exit.success() { 0 } else { 1 },
        "task_exit": if task.exit.success() { 0 } else { 1 },
        "output_bytes": output_bytes,
        "removed": removed,
    });
    let text = serde_json::to_vec_pretty(&value).unwrap();
    assert!(!String::from_utf8_lossy(&text).contains(&key));
    std::fs::write(file, text).unwrap();
}

#[test]
fn claude_published_workload_ready_and_headless_task() {
    smoke(Case {
        test: "sbx_smoke::claude_published_workload_ready_and_headless_task",
        agent: "claude",
        disposition: "published-workload-with-mixin",
        kit_handle: "awman-claude-mixin",
    });
}

#[test]
fn opencode_owned_workload_ready_and_headless_task() {
    smoke(Case {
        test: "sbx_smoke::opencode_owned_workload_ready_and_headless_task",
        agent: "opencode",
        disposition: "owned-workload",
        kit_handle: "awman-opencode",
    });
}
