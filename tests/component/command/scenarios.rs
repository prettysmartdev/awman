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
    // Successful check-only scenarios now include the selected agent image.
    // Other images retain the baseline's missing-image response.
    for repo in ["workflow", "read-only"] {
        let tag = awman::data::image_tags::agent_image_tag(&s.work().join(repo), "claude");
        docker.on(&["image", "inspect", &tag]).exit(0);
    }
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

fn container_env(backend: &Backend, s: &TestSandbox) -> awman_test_support::EnvMap {
    let mut env = backend.env();
    // The fake binaries and scratch directory are siblings. Admit only the
    // sandbox's fakes through host CLI isolation, as in the container suites.
    for key in ["TMPDIR", "TMP", "TEMP"] {
        env.set(key, s.root());
    }
    env
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
        .step_with(awman_test_support::fixtures::WorkflowStep {
            name: "first".into(),
            agent: Some("claude".into()),
            prompt: "literal $(codex)".into(),
            ..Default::default()
        })
        .write(repo.path(), "workflow");
    let path = workflow.to_str().unwrap();
    let before = std::fs::read(&workflow).unwrap();
    let checked = crate::support::awman(s)
        .envs(&container_env(backend, s))
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
    assert!(report["prerequisites"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| {
            check["prerequisite"]["type"] == "agent_image"
                && check["prerequisite"]["agent"] == "claude"
                && check["status"] == "satisfied"
        }));
    assert_eq!(std::fs::read(&workflow).unwrap(), before);
    let missing = crate::support::awman(s)
        .envs(&container_env(backend, s))
        .args(["exec", "workflow", "missing.toml", "--headless"])
        .current_dir(repo.path())
        .run();
    assert_eq!(missing.exit, Exit::Code(4), "{}", missing.stderr_text());
    json!({"preflight_exit":0,"missing_exit":4,"workflow_unchanged":true})
}

/// WI 0152: the CLI refuses the read-only option in the same typed way on
/// every backend, before any container is launched, and the help text names
/// it. The trace is compared across backends by the differential test.
pub async fn read_only_workspace_refusals_are_typed_and_precede_launch(
    backend: &Backend,
    s: &TestSandbox,
) -> Value {
    let repo = RepoBuilder::new(s, "read-only")
        .file("README.md", "fixture\n")
        .file(
            "setup.toml",
            "title = 's'\nagent = 'claude'\n\n[[setup]]\ntype = 'run_shell'\ncommand = 'true'\n\n[[step]]\nname = 'run'\nprompt = 'look'\n",
        )
        .file(
            "plain.toml",
            "title = 'p'\nagent = 'claude'\n\n[[step]]\nname = 'run'\nprompt = 'look'\n",
        )
        .init();
    let run = |args: &[&str]| {
        let out = crate::support::awman(s)
            .envs(&container_env(backend, s))
            .args(args)
            .current_dir(repo.path())
            .run();
        let text = format!("{}{}", out.stdout_text(), out.stderr_text());
        (out.exit, text)
    };
    let help = run(&["exec", "prompt", "--help"]);
    assert_eq!(help.0, Exit::Code(0), "{}", help.1);
    assert!(help.1.contains("--read-only-workspace"), "{}", help.1);

    let docker = run(&[
        "exec",
        "prompt",
        "--read-only-workspace",
        "--allow-docker",
        "look",
    ]);
    assert_ne!(docker.0, Exit::Code(0), "{}", docker.1);
    assert!(
        docker.1.contains("read-only-workspace") && docker.1.contains("allow-docker"),
        "{}",
        docker.1
    );

    let dynamic = run(&[
        "exec",
        "workflow",
        "--dynamic",
        "--read-only-workspace",
        "--headless",
    ]);
    assert_ne!(dynamic.0, Exit::Code(0), "{}", dynamic.1);
    assert!(dynamic.1.contains("--dynamic"), "{}", dynamic.1);

    let setup = run(&[
        "exec",
        "workflow",
        "setup.toml",
        "--read-only-workspace",
        "--headless",
    ]);
    assert_ne!(setup.0, Exit::Code(0), "{}", setup.1);
    assert!(setup.1.contains("setup or teardown"), "{}", setup.1);

    // The option is accepted by the preflight-only path of a plain workflow.
    let ok = run(&[
        "exec",
        "workflow",
        "plain.toml",
        "--read-only-workspace",
        "--check-prerequisites",
        "--headless",
        "--json",
    ]);
    assert_eq!(ok.0, Exit::Code(0), "{}", ok.1);
    json!({
        "help_names_flag": true,
        "allow_docker_refused": true,
        "dynamic_refused": true,
        "setup_refused": true,
        "check_exit": 0,
    })
}
