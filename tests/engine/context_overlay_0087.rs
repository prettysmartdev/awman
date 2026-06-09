//! Integration tests for WI-0087 context overlay feature.
//!
//! Covers the cross-layer flows described in the WI-0087 test considerations:
//! exec_prompt + context(global) pipeline, workflow overlay merging, and the
//! context(workflow) dynamic prompt step-progression markers.

use awman::command::commands::{collect_all_overlay_specs, ContextScope};
use awman::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
use awman::data::fs::ContextDirResolver;
use awman::data::session::{AgentName, Session, SessionOpenOptions, StaticGitRootResolver};
use awman::engine::agent::{AgentEngine, AgentRunOptions};
use awman::engine::container::options::{ContainerOption, OverlayPermission};
use awman::engine::context_prompt::{ContextPromptBuilder, WorkflowStepInfo, WorkflowStepState};
use awman::engine::overlay::{ContextOverlay, ContextScope as EngineContextScope, OverlayEngine};

use std::path::PathBuf;
use std::sync::Arc;

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn open_session_with_env(git_root: &std::path::Path, env: EnvSnapshot) -> Session {
    let resolver = StaticGitRootResolver::new(git_root);
    let opts = SessionOpenOptions {
        flags: Default::default(),
        env: Some(env),
        available_agents: None,
    };
    Session::open(git_root.to_path_buf(), &resolver, opts).expect("Session::open")
}

fn make_agent_engine(home: &std::path::Path) -> AgentEngine {
    let overlay = OverlayEngine::with_auth_resolver(
        awman::data::fs::auth_paths::AuthPathResolver::at_home(home),
    )
    .with_secret_files_provider(Arc::new(|_| Vec::new()));
    let runtime = awman::engine::container::ContainerRuntime::docker();
    AgentEngine::new(Arc::new(overlay), Arc::new(runtime))
}

// ─── Integration: exec_prompt with context(global) pipeline ──────────────────

#[test]
fn exec_prompt_context_global_emits_overlay_at_awman_context_global() {
    let tmp = tempfile::tempdir().unwrap();
    // Use ContextDirResolver::at_home to deterministically resolve the host path.
    let resolver = ContextDirResolver::at_home(tmp.path());
    let global_dir = resolver.global_dir();
    ContextDirResolver::ensure_exists(&global_dir).unwrap();

    let context_overlay = ContextOverlay {
        scope: EngineContextScope::Global,
        host_path: global_dir.clone(),
        container_path: PathBuf::from("/awman/context/global"),
        permission: OverlayPermission::ReadWrite,
    };

    let session_tmp = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
    let session = open_session_with_env(session_tmp.path(), env);
    let engine = make_agent_engine(tmp.path());
    let agent = AgentName::new("claude").unwrap();

    let system_prompt = ContextPromptBuilder::new().with_global().build();
    let run = AgentRunOptions {
        context_overlays: vec![context_overlay],
        system_prompt,
        ..Default::default()
    };

    let opts = engine.build_options(&session, &agent, &run).unwrap();

    // Must have a context directory mount at /awman/context/global.
    let has_ctx_overlay = opts.iter().any(|o| {
        if let ContainerOption::Overlay(spec) = o {
            spec.container_path == std::path::Path::new("/awman/context/global")
        } else {
            false
        }
    });
    assert!(
        has_ctx_overlay,
        "exec_prompt with context(global) must emit Overlay at /awman/context/global; \
         got {opts:?}"
    );

    // Must have a system-prompt delivery option (claude = SystemPromptFile).
    let has_system_prompt = opts
        .iter()
        .any(|o| matches!(o, ContainerOption::SystemPromptFile { .. }));
    assert!(
        has_system_prompt,
        "exec_prompt with context(global) + system_prompt must emit SystemPromptFile; \
         got {opts:?}"
    );
}

// ─── Integration: workflow overlay merging ────────────────────────────────────

#[test]
fn workflow_top_level_context_repo_and_step_context_global_produce_two_specs() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
    let session = open_session_with_env(tmp.path(), env);

    let workflow_overlays = vec!["context(repo)".to_string()];
    let step_overlays = vec!["context(global)".to_string()];

    let collected = collect_all_overlay_specs(
        &session,
        vec![],
        Some(&workflow_overlays),
        Some(&step_overlays),
    )
    .unwrap();

    assert_eq!(
        collected.context_overlays.len(),
        2,
        "workflow-level context(repo) + step-level context(global) must produce \
         two ContextOverlaySpec entries; got {:?}",
        collected.context_overlays
    );

    let has_repo = collected
        .context_overlays
        .iter()
        .any(|c| c.scope == ContextScope::Repo);
    let has_global = collected
        .context_overlays
        .iter()
        .any(|c| c.scope == ContextScope::Global);

    assert!(
        has_repo,
        "Repo scope must be present from workflow-level overlays"
    );
    assert!(
        has_global,
        "Global scope must be present from step-level overlays"
    );
}

#[test]
fn workflow_context_global_at_top_level_plus_step_context_workflow_union_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
    let session = open_session_with_env(tmp.path(), env);

    let workflow_overlays = vec!["context(global)".to_string()];
    let step_overlays = vec!["context(workflow)".to_string()];

    let collected = collect_all_overlay_specs(
        &session,
        vec![],
        Some(&workflow_overlays),
        Some(&step_overlays),
    )
    .unwrap();

    assert_eq!(
        collected.context_overlays.len(),
        2,
        "workflow-level context(global) + step-level context(workflow) must produce \
         two context specs (union semantics); got {:?}",
        collected.context_overlays
    );
}

// ─── Integration: context(workflow) dynamic prompt step markers ───────────────

#[test]
fn workflow_dynamic_prompt_second_step_shows_first_completed_second_in_progress() {
    let info = WorkflowStepInfo {
        workflow_title: "Release Pipeline".to_string(),
        current_step_name: "build-and-test".to_string(),
        current_step_index: 1,
        total_steps: 2,
        steps: vec![
            (
                "prepare-environment".to_string(),
                WorkflowStepState::Completed,
            ),
            ("build-and-test".to_string(), WorkflowStepState::InProgress),
        ],
        work_item_number: None,
        work_item_title: None,
    };

    let prompt = ContextPromptBuilder::new()
        .with_workflow(&info)
        .build()
        .expect("prompt must be Some when workflow scope is active");

    assert!(
        prompt.contains("[✓]") && prompt.contains("prepare-environment"),
        "first step must be marked [✓] completed; got: {prompt}"
    );
    assert!(
        prompt.contains("[→]") && prompt.contains("build-and-test"),
        "second step must be marked [→] in progress; got: {prompt}"
    );
    assert!(
        prompt.contains("Release Pipeline"),
        "prompt must contain the workflow title; got: {prompt}"
    );
    assert!(
        prompt.contains("step 2 of 2"),
        "prompt must show current step index (step 2 of 2); got: {prompt}"
    );
}

#[test]
fn workflow_dynamic_prompt_three_steps_all_markers_present() {
    let info = WorkflowStepInfo {
        workflow_title: "Workflow".to_string(),
        current_step_name: "middle".to_string(),
        current_step_index: 1,
        total_steps: 3,
        steps: vec![
            ("first".to_string(), WorkflowStepState::Completed),
            ("middle".to_string(), WorkflowStepState::InProgress),
            ("last".to_string(), WorkflowStepState::Pending),
        ],
        work_item_number: None,
        work_item_title: None,
    };

    let prompt = ContextPromptBuilder::new()
        .with_workflow(&info)
        .build()
        .expect("prompt must be Some");

    assert!(prompt.contains("[✓]"), "completed marker [✓] must appear");
    assert!(prompt.contains("[→]"), "in-progress marker [→] must appear");
    assert!(prompt.contains("[○]"), "pending marker [○] must appear");
    assert!(prompt.contains("first"), "step name 'first' must appear");
    assert!(prompt.contains("middle"), "step name 'middle' must appear");
    assert!(prompt.contains("last"), "step name 'last' must appear");
}
