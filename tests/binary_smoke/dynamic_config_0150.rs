//! WI 0150 — `workflows.dynamic` end to end through the real binary.
//!
//! Each test builds a throwaway git repo and `HOME`, puts the fake container
//! CLI from `helpers/api_process.rs` first on `PATH` (so no Docker is ever
//! reached) and runs `awman exec workflow --dynamic`. The fake docker is
//! extended to record the leader's stdin, which is the leader prompt, so the
//! tests compare the prompt a real run would hand the leader.

#[path = "../helpers/api_process.rs"]
mod api_process;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The prompt sections the config feeds: from the agents listing through the
/// guidance block (the concurrency note sits between them), which ends where
/// the template's workflow-design section begins.
fn config_fed_section(prompt: &str) -> &str {
    let start = prompt
        .find("## Available Agents and Models")
        .expect("agents heading");
    let end = prompt
        .find("## Designing the Workflow")
        .expect("design heading");
    &prompt[start..end]
}

/// The same section rendered straight from the template, for a golden check.
fn golden_section(agents: &str, max: Option<usize>, guidance: &[&str]) -> String {
    let guidance: Vec<String> = guidance.iter().map(|g| g.to_string()).collect();
    let rendered = awman::data::dynamic_workflow_assets::build_leader_prompt(
        "0001",
        "/workspace/aspec/work-items/0001-x.md",
        agents,
        max,
        Some(&guidance),
    );
    config_fed_section(&rendered).to_string()
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// A committed repo whose `.awman/config.json` is `repo_config` (when
    /// given) and whose global config is `global_config` (when given).
    fn new(repo_config: Option<&str>, global_config: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for sub in ["home/.awman", "bin", "control", "repo/.awman"] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
        }
        let fake = api_process::FAKE_DOCKER.replacen(
            "if [ -n \"$leader\" ]; then\n",
            "if [ -n \"$leader\" ]; then\n      cat > \"$ctl/leader-prompt.txt\"\n",
            1,
        );
        assert_ne!(fake, api_process::FAKE_DOCKER, "fake docker patch applied");
        std::fs::write(p.join("bin/docker"), fake).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(p.join("bin/docker"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(p.join("control/mode"), "edit").unwrap();

        let repo = p.join("repo");
        std::fs::create_dir_all(repo.join("aspec/work-items")).unwrap();
        std::fs::write(repo.join("aspec/work-items/0001-x.md"), "# wi\n").unwrap();
        std::fs::write(repo.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
        if let Some(c) = repo_config {
            std::fs::write(repo.join(".awman/config.json"), c).unwrap();
        }
        if let Some(c) = global_config {
            std::fs::write(p.join("home/.awman/config.json"), c).unwrap();
        }
        let fx = Self { dir };
        fx.git(&["init", "-q", "."]);
        fx.git(&["add", "-A"]);
        fx.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "i",
        ]);
        fx
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn repo_config_path(&self) -> PathBuf {
        self.repo().join(".awman/config.json")
    }

    fn repo_config(&self) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.repo_config_path()).unwrap()).unwrap()
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.repo())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn awman(&self, args: &[&str]) -> Output {
        let p: &Path = self.dir.path();
        let path = format!(
            "{}:{}",
            p.join("bin").display(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
        );
        Command::new(crate::awman_bin())
            .args(args)
            .current_dir(self.repo())
            .env_clear()
            .env("PATH", path)
            .env("HOME", p.join("home"))
            .env("TMPDIR", std::env::temp_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("AWMAN_TEST_ISOLATION", "1")
            .env("FAKE_DOCKER_CONTROL", p.join("control"))
            .stdin(Stdio::null())
            .output()
            .expect("run awman")
    }

    /// Run the dynamic workflow and return (leader prompt, stderr).
    fn run_dynamic(&self) -> (String, String) {
        let _ = std::fs::remove_file(self.dir.path().join("control/leader-prompt.txt"));
        let out = self.awman(&[
            "exec",
            "workflow",
            "--dynamic",
            "--work-item",
            "1",
            "--non-interactive",
            "--on-existing-worktree",
            "recreate",
        ]);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(out.status.success(), "dynamic run failed: {stderr}");
        let prompt = std::fs::read_to_string(self.dir.path().join("control/leader-prompt.txt"))
            .unwrap_or_else(|e| panic!("no leader prompt captured ({e}): {stderr}"));
        (prompt, stderr)
    }

    fn docker_log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("control/docker.log")).unwrap_or_default()
    }
}

const LEGACY_REPO: &str = r#"{"agent":"claude","dynamicWorkflows":{"guidance":["be nice","use tabs"],"maxConcurrentSteps":2}}"#;
const NEW_REPO: &str = r#"{"agent":"claude","workflows":{"dynamic":{"guidance":["be nice","use tabs"],"maxConcurrentSteps":2}}}"#;
const DEPRECATION: &str = "uses `dynamicWorkflows`, which has moved to `workflows.dynamic`";

fn deprecation_count(stderr: &str) -> usize {
    stderr.matches(DEPRECATION).count()
}

// ─── legacy-only repo, no global block ──────────────────────────────────────

#[test]
fn legacy_only_repo_gives_the_same_leader_prompt_as_the_new_location() {
    let legacy = Fixture::new(Some(LEGACY_REPO), None);
    let (legacy_prompt, stderr) = legacy.run_dynamic();
    assert_eq!(deprecation_count(&stderr), 1, "warned once: {stderr}");

    let modern = Fixture::new(Some(NEW_REPO), None);
    let (modern_prompt, stderr) = modern.run_dynamic();
    assert_eq!(deprecation_count(&stderr), 0, "{stderr}");

    assert_eq!(legacy_prompt, modern_prompt, "byte-identical leader prompt");
}

#[test]
fn legacy_only_repo_prompt_carries_the_guidance_block_and_concurrency_note() {
    let fx = Fixture::new(Some(LEGACY_REPO), None);
    let (prompt, _) = fx.run_dynamic();
    let section = config_fed_section(&prompt);
    assert!(
        section.contains("Maximum concurrent steps advised: 2."),
        "{section}"
    );
    assert!(section.contains("- be nice\n- use tabs\n"), "{section}");

    // Everything the config feeds matches the template rendered directly.
    let agents_start = section.find("Each agent is a Docker container").unwrap();
    let agents_end = section.find("Maximum concurrent steps advised").unwrap();
    let agents = section[agents_start..agents_end].trim_end();
    let golden = golden_section(agents, Some(2), &["be nice", "use tabs"]);
    let blocks = |s: &str| s[s.find("Maximum concurrent steps advised").unwrap()..].to_string();
    assert_eq!(blocks(section), blocks(&golden));
}

// ─── global-only block ──────────────────────────────────────────────────────

#[test]
fn global_only_block_applies_the_global_leader_guidance_and_concurrency() {
    let global = r#"{"workflows":{"dynamic":{"defaultLeader":"claude::sonnet-global","maxConcurrentSteps":3,"guidance":["global rule"]}}}"#;
    let fx = Fixture::new(Some(r#"{"agent":"claude"}"#), Some(global));
    let (prompt, stderr) = fx.run_dynamic();
    let section = config_fed_section(&prompt);
    assert!(section.contains("advised: 3."), "{section}");
    assert!(section.contains("- global rule"), "{section}");
    assert!(
        fx.docker_log().contains("sonnet-global"),
        "global leader model reaches the container: {}",
        fx.docker_log()
    );
    assert_eq!(deprecation_count(&stderr), 0, "{stderr}");
}

// ─── global + repo ──────────────────────────────────────────────────────────

#[test]
fn global_plus_repo_merges_guidance_and_uses_the_repo_agent_map() {
    let global = r#"{"workflows":{"dynamic":{"maxConcurrentSteps":5,"guidance":["first","shared"],"agentsToModels":{"claude":["global-model"],"codex":["codex-model"]}}}}"#;
    let repo = r#"{"agent":"claude","workflows":{"dynamic":{"guidance":["shared","last"],"agentsToModels":{"claude":["repo-model"]}}}}"#;
    let fx = Fixture::new(Some(repo), Some(global));
    let (prompt, _) = fx.run_dynamic();
    let section = config_fed_section(&prompt);

    // Global entries first, then the repo's; the duplicate appears once.
    assert!(section.contains("- first\n- shared\n- last\n"), "{section}");
    assert_eq!(section.matches("- shared").count(), 1, "{section}");
    // maxConcurrentSteps falls through from the global scope.
    assert!(section.contains("advised: 5."), "{section}");
    // The non-empty repo map replaces the global one outright.
    assert!(section.contains("repo-model"), "{section}");
    assert!(!section.contains("global-model"), "{section}");
    assert!(!section.contains("codex"), "{section}");
}

// ─── migrate, then the same run ─────────────────────────────────────────────

#[test]
fn config_migrate_yes_then_the_same_run_gives_the_identical_prompt() {
    let fx = Fixture::new(Some(LEGACY_REPO), None);
    let (before, _) = fx.run_dynamic();

    let out = fx.awman(&["config", "migrate", "--yes"]);
    assert!(out.status.success(), "{out:?}");
    let config = fx.repo_config();
    assert!(config.get("dynamicWorkflows").is_none(), "{config}");
    assert_eq!(config["workflows"]["dynamic"]["maxConcurrentSteps"], 2);
    assert_eq!(config["agent"], "claude", "unrelated keys kept");
    // The migrated file is no longer committed-clean, but the run reads the
    // working copy; commit it so the worktree matches.
    fx.git(&["add", "-A"]);
    fx.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "m",
    ]);

    let (after, stderr) = fx.run_dynamic();
    assert_eq!(deprecation_count(&stderr), 0, "{stderr}");
    assert_eq!(
        before, after,
        "byte-identical leader prompt after migrating"
    );
}

// ─── older-format file + config set ─────────────────────────────────────────

#[test]
fn older_format_repo_file_is_migrated_by_an_unrelated_config_set() {
    let older = r#"{"agent":"claude","dynamicWorkflows":{"guidance":["keep me"],"agentsToModels":{"claude":["m1"]}}}"#;
    let fx = Fixture::new(Some(older), None);
    let out = fx.awman(&["config", "set", "agent", "codex"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    let config = fx.repo_config();
    assert!(config.get("dynamicWorkflows").is_none(), "{config}");
    assert_eq!(config["workflows"]["dynamic"]["guidance"][0], "keep me");
    assert_eq!(
        config["workflows"]["dynamic"]["agentsToModels"]["claude"][0],
        "m1"
    );
    assert_eq!(config["agent"], "codex", "{config}");
    assert!(
        text.contains("workflows.dynamic"),
        "the move is reported: {text}"
    );
}

// ─── `config migrate` through the CLI ───────────────────────────────────────

#[test]
fn config_migrate_dry_run_writes_nothing() {
    let fx = Fixture::new(Some(LEGACY_REPO), None);
    let out = fx.awman(&["config", "migrate", "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("dry run: nothing written"));
    assert_eq!(
        std::fs::read_to_string(fx.repo_config_path()).unwrap(),
        LEGACY_REPO
    );
}

#[test]
fn config_migrate_non_interactive_without_yes_exits_2_and_writes_nothing() {
    let fx = Fixture::new(Some(LEGACY_REPO), None);
    let out = fx.awman(&["config", "migrate", "--non-interactive"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(fx.repo_config_path()).unwrap(),
        LEGACY_REPO
    );
}

#[test]
fn config_migrate_with_nothing_to_migrate_exits_zero() {
    let fx = Fixture::new(Some(NEW_REPO), None);
    let out = fx.awman(&["config", "migrate", "--yes"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing to migrate"));
    assert_eq!(
        std::fs::read_to_string(fx.repo_config_path()).unwrap(),
        NEW_REPO
    );
}

#[test]
fn config_set_work_items_dir_persists_on_a_current_format_file() {
    let fx = Fixture::new(Some(NEW_REPO), None);
    let out = fx.awman(&["config", "set", "work_items.dir", "x"]);
    assert!(out.status.success(), "{out:?}");
    let config = fx.repo_config();
    assert_eq!(config["workItems"]["dir"], "x", "{config}");
}
