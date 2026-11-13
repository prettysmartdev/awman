//! Both backends call these exact assertions; differential compares the traces.
use crate::support::backend::Backend;
use awman_test_support::{
    fixtures::{ConfigTree, RepoBuilder, WorkflowBuilder},
    Exit, TestSandbox,
};
use serde_json::{json, Value};

pub fn prepare(s: &TestSandbox) -> Vec<awman_test_support::fakes::FakeCli> {
    ConfigTree::new()
        .global(
            json!({"runtime":{"kind":"docker"},"agents":{"auth":{"refresh":{"enabled":false}}}}),
        )
        .write(s, None);
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    [
        "claude", "codex", "gemini", "opencode", "pi", "crush", "maki",
    ]
    .into_iter()
    .map(|name| s.install_fake(crate::support::fake_cli_exe(), name))
    .collect()
}

pub fn assert_no_host_agents(agents: &[awman_test_support::fakes::FakeCli]) {
    for agent in agents {
        assert!(
            agent.calls().is_empty(),
            "host agent was invoked: {:?}",
            agent.calls()
        );
    }
}

pub async fn catalogue_explains_each_command_before_execution(
    backend: &Backend,
    s: &TestSandbox,
) -> Value {
    let mut trace = Vec::new();
    for path in super::catalogue::CONTRACT_PATHS {
        let args: Vec<_> = path.split_whitespace().chain(["--help"]).collect();
        let output = crate::support::awman(s)
            .envs(&backend.env())
            .args(args)
            .current_dir(s.work())
            .run();
        assert_eq!(
            output.exit,
            Exit::Code(0),
            "{path}: {}",
            output.stderr_text()
        );
        assert!(
            output.stdout_text().contains("Usage:"),
            "{path}: {}",
            output.stderr_text()
        );
        trace.push(json!({"command":path,"exit":0,"usage":true}));
    }
    json!(trace)
}

pub async fn config_edits_are_durable_and_invalid_values_are_atomic(
    backend: &Backend,
    s: &TestSandbox,
) -> Value {
    let run = |args: &[&str]| {
        crate::support::awman(s)
            .envs(&backend.env())
            .args(args.iter().copied())
            .current_dir(s.work())
            .run()
    };
    run(&["config", "set", "agents.default", "codex", "--global"]).assert_success();
    let before = std::fs::read(s.awman_home().join("config.json")).unwrap();
    let rejected = run(&[
        "config",
        "set",
        "daemon.features.worker.enabled",
        "maybe",
        "--global",
    ]);
    assert_ne!(rejected.exit, Exit::Code(0));
    assert_eq!(
        std::fs::read(s.awman_home().join("config.json")).unwrap(),
        before
    );
    let output = run(&["config", "get", "agents.default", "--json"]);
    output.assert_success();
    let value: Value = serde_json::from_str(&output.stdout_text()).unwrap();
    assert!(output.stdout_text().contains("codex"), "{value}");
    run(&["config", "unset", "agents.default", "--global"]).assert_success();
    let saved: Value =
        serde_json::from_slice(&std::fs::read(s.awman_home().join("config.json")).unwrap())
            .unwrap();
    assert!(saved.pointer("/agents/default").is_none());
    json!({"set":"codex","invalid":"unchanged","unset":true})
}

pub async fn workflow_preflight_is_read_only_and_missing_work_is_typed(
    backend: &Backend,
    s: &TestSandbox,
) -> Value {
    let repo = RepoBuilder::new(s, "workflow")
        .file("README.md", "fixture\n")
        .init();
    let workflow = WorkflowBuilder::new("preflight")
        .step("first", "literal $(codex)", &[])
        .write(repo.path(), "workflow");
    let path = workflow.to_str().unwrap();
    let before = std::fs::read(&workflow).unwrap();
    let checked = crate::support::awman(s)
        .envs(&backend.env())
        .args([
            "exec",
            "workflow",
            path,
            "--check-prerequisites",
            "--headless",
            "--json",
        ])
        .current_dir(repo.path())
        .run();
    assert_eq!(
        checked.exit,
        Exit::Code(0),
        "{} {}",
        checked.stdout_text(),
        checked.stderr_text()
    );
    let report: Value = serde_json::from_str(&checked.stdout_text()).unwrap();
    assert!(report.is_object());
    assert_eq!(std::fs::read(&workflow).unwrap(), before);
    let missing = crate::support::awman(s)
        .envs(&backend.env())
        .args(["exec", "workflow", "missing.toml", "--headless"])
        .current_dir(repo.path())
        .run();
    assert_eq!(missing.exit, Exit::Code(4), "{}", missing.stderr_text());
    json!({"preflight_exit":0,"missing_exit":4,"workflow_unchanged":true})
}
