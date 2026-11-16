// These contracts combine session loading, configuration and workflow preflight.
// Keep them in the component suite for both default and builtin feature builds.
use super::*;
use awman_test_support::{fixtures::ConfigTree, TestCase, TestSandbox};
use serde_json::json;

fn sandbox(id: &'static str) -> TestSandbox {
    TestSandbox::for_test(TestCase::component(id))
}

fn session(s: &TestSandbox) -> Session {
    Session::open_at_directory_root(
        s.work(),
        crate::data::session::SessionOpenOptions {
            env: Some(crate::data::config::env::EnvSnapshot::from_full_env(
                s.env()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into_owned(),
                            v.to_string_lossy().into_owned(),
                        )
                    })
                    .collect(),
            )),
            ..Default::default()
        },
    )
    .unwrap()
}

fn flags() -> ExecWorkflowCommandFlags {
    ExecWorkflowCommandFlags {
        workflow: None,
        work_item: Some("7".into()),
        non_interactive: true,
        plan: false,
        allow_docker: false,
        worktree: false,
        yolo: false,
        auto: false,
        agent: Some("claude".into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        max_concurrent: None,
        dynamic: true,
        leader: None,
        on_dirty_merge_target: None,
        on_merge_failure: None,
        on_existing_worktree: None,
        prerequisites: Default::default(),
        squad_task_prerequisites: None,
    }
}

#[test]
fn missing_project_dockerfiles_list_every_agent_and_init_action_without_writes() {
    let s = sandbox("command::commands::exec_workflow::dockerfile_preflight_contracts::missing_project_dockerfiles_list_every_agent_and_init_action_without_writes");
    ConfigTree::new()
        .repo(json!({"workflows":{"dynamic":{"agentsToModels":{
            "codex":["model"], "claude":["model"]
        }}}}))
        .write(&s, Some(&s.work()));
    let session = session(&s);
    let flags = flags();
    let dynamic = resolve_dynamic_workflows(&session, &flags).unwrap();
    let config_path = s.work().join(".awman/config.json");
    let before = std::fs::read(&config_path).unwrap();
    let error = validate_dynamic_agent_dockerfiles(&session, &flags, &dynamic)
        .unwrap_err()
        .to_string();
    assert_eq!(error, concat!(
        "dynamic workflow references agents with no Dockerfile in the project:\n",
        "  - \"claude\" (expected .awman/Dockerfile.claude)\n",
        "  - \"codex\" (expected .awman/Dockerfile.codex)\n",
        "Run init for each missing agent before retrying:\n",
        "  POST /v1/worker/commands {\"subcommand\":\"init\",\"args\":[\"--agent\",\"claude\"]} / awman init --agent claude\n",
        "  POST /v1/worker/commands {\"subcommand\":\"init\",\"args\":[\"--agent\",\"codex\"]} / awman init --agent codex"
    ));
    assert_eq!(std::fs::read(config_path).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(s.work().join(".awman")).unwrap().count(),
        1
    );
    for agent in ["claude", "codex"] {
        std::fs::write(
            s.work().join(format!(".awman/Dockerfile.{agent}")),
            "FROM scratch\n",
        )
        .unwrap();
    }
    validate_dynamic_agent_dockerfiles(&session, &flags, &dynamic).unwrap();
    s.finish();
}

#[test]
fn prepared_agents_pass_and_global_model_maps_still_skip_missing_agents() {
    let s = sandbox("command::commands::exec_workflow::dockerfile_preflight_contracts::prepared_agents_pass_and_global_model_maps_still_skip_missing_agents");
    ConfigTree::new()
        .global(json!({"workflows":{"dynamic":{"agentsToModels":{
            "codex":["model"]
        }}}}))
        .write(&s, None);
    std::fs::create_dir_all(s.work().join(".awman")).unwrap();
    // Even an empty existing recipe is project-owned, never replaced here.
    std::fs::write(s.work().join(".awman/Dockerfile.claude"), "").unwrap();
    let session = session(&s);
    let flags = flags();
    let dynamic = resolve_dynamic_workflows(&session, &flags).unwrap();
    validate_dynamic_agent_dockerfiles(&session, &flags, &dynamic).unwrap();
    let paths = crate::data::RepoDockerfilePaths::new(s.work());
    let (_, _, warnings) =
        agents_section_for_leader(&dynamic, &paths.discover_agent_dockerfiles()).unwrap();
    assert!(warnings.iter().any(|w| w.contains("skipping 'codex'")));
    assert!(!paths.agent_dockerfile("codex").exists());
    let mut flags = flags;
    flags.leader = Some("codex::model".into());
    assert!(validate_dynamic_agent_dockerfiles(&session, &flags, &dynamic).is_err());
    std::fs::write(paths.agent_dockerfile("codex"), "FROM scratch\n").unwrap();
    validate_dynamic_agent_dockerfiles(&session, &flags, &dynamic).unwrap();
    s.finish();
}

#[test]
fn generated_workflow_missing_dockerfiles_do_not_consume_leader_repairs() {
    let s = sandbox("command::commands::exec_workflow::dockerfile_preflight_contracts::generated_workflow_missing_dockerfiles_do_not_consume_leader_repairs");
    let session = session(&s);
    let path = s.work().join("workflow.toml");
    let content = "[[step]]\nname = 'work'\nagent = 'claude'\nprompt = 'work'\n";
    std::fs::write(&path, content).unwrap();
    let paths = crate::data::RepoDockerfilePaths::new(s.work());
    let mut repair = WorkflowRepairLoop::new(&path, "design");
    let RepairDecision::Accepted(workflow) =
        repair.record(validate_generated_workflow(&path, &session, &paths))
    else {
        panic!("project setup must not be sent to the leader for repair");
    };
    let error = resolve_and_validate_workflow_agents(&workflow, &session, &paths).unwrap_err();
    assert!(error.starts_with("workflow.toml references agents with no Dockerfile in the project:"));
    assert!(error.contains("POST /v1/worker/commands"));
    assert_eq!(repair.attempt(), 0);
    assert_eq!(repair.prompt(), "design");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    assert!(!s.work().join(".awman").exists());
    // Invalid leader output still consumes a repair, preserving the budget.
    std::fs::write(&path, "invalid TOML").unwrap();
    assert!(matches!(
        repair.record(validate_generated_workflow(&path, &session, &paths)),
        RepairDecision::Retry { attempt: 1, .. }
    ));
    s.finish();
}
