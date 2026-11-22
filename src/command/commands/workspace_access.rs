//! Read-only workspace policy for agent launches (WI 0152) — Layer 2.
//!
//! `exec prompt --read-only-workspace`, `exec workflow --read-only-workspace`,
//! a workflow step's `read_only_workspace` and a pulled lease's
//! `exec.read_only` all end here. This module decides:
//!
//! - whether the run may start at all (the preflight: the runtime must
//!   enforce a read-only mount, and `--allow-docker` is refused with it),
//! - which agent engine launches the agent (a read-only clone; never a
//!   writable one when a lease already asked for read-only),
//! - the one system-prompt section that tells the agent where it may write,
//! - the one info message a frontend shows.
//!
//! Layer 1 enforces the mount itself (`AgentEngine::with_read_only_workspace`)
//! and refuses again if it cannot; nothing here ever turns a read-only
//! request into a writable launch.

use std::sync::Arc;

use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::workflow_definition::Workflow;
use crate::data::workspace::{WorkspaceAccess, READ_ONLY_WORKSPACE_FLAG};
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::container::options::OverlayPermission;
use crate::engine::overlay::ContextOverlay;

/// Why `--allow-docker` is refused with a read-only workspace.
pub const ALLOW_DOCKER_REASON: &str =
    "--allow-docker gives the agent the host Docker daemon, which can write to the workspace";

/// Why `exec workflow --read-only-workspace` is refused with `--dynamic`.
pub const DYNAMIC_REASON: &str = "--dynamic generates its workflow at run time, so its steps cannot be checked before the run; mark individual steps read_only_workspace = true in a workflow file instead";

/// Refusal for a command-level `--read-only-workspace` on a workflow that
/// has host-authored setup or teardown steps (they run with a writable
/// workspace).
pub const SETUP_TEARDOWN_REFUSAL: &str = "--read-only-workspace cannot be used with a workflow that has setup or teardown steps; mark individual steps read_only_workspace = true instead";

/// Fail before any side effect when `runtime` cannot enforce `access`.
///
/// `ReadWrite` always passes. `ReadOnly` fails with
/// [`CommandError::ReadOnlyWorkspaceUnsupported`] on a runtime whose
/// capabilities say it cannot mount the workspace read-only, and with
/// [`CommandError::InvalidFlagValue`] when `--allow-docker` is also set.
pub fn preflight_workspace_access(
    access: WorkspaceAccess,
    allow_docker: bool,
    runtime: &dyn AgentRuntimeEngine,
) -> Result<(), CommandError> {
    check_workspace_access(
        access,
        allow_docker,
        runtime.capabilities().read_only_workspace(),
        runtime.display_name(),
    )
}

/// [`preflight_workspace_access`] on plain values, so the rule is testable
/// without a runtime.
pub(crate) fn check_workspace_access(
    access: WorkspaceAccess,
    allow_docker: bool,
    runtime_supports_read_only: bool,
    runtime_name: &str,
) -> Result<(), CommandError> {
    if !access.is_read_only() {
        return Ok(());
    }
    if allow_docker {
        return Err(CommandError::InvalidFlagValue {
            command: Vec::new(),
            flag: READ_ONLY_WORKSPACE_FLAG.to_string(),
            reason: ALLOW_DOCKER_REASON.to_string(),
        });
    }
    if !runtime_supports_read_only {
        return Err(CommandError::ReadOnlyWorkspaceUnsupported {
            runtime: runtime_name.to_string(),
        });
    }
    Ok(())
}

/// The workflow-wide preflight of `exec workflow`, run once the workflow
/// file is known and before setup, any worktree or any container.
///
/// `command` is the command-level flag. The strictest access any agent step
/// of the run can have is checked against the runtime, so a workflow with a
/// single read-only step fails before step 1 on a runtime that cannot honour
/// it, not midway. With the command-level flag, a workflow that declares
/// setup or teardown entries is refused: those run with a writable workspace.
pub fn preflight_workflow_access(
    workflow: &Workflow,
    command: WorkspaceAccess,
    allow_docker: bool,
    runtime: &dyn AgentRuntimeEngine,
) -> Result<(), CommandError> {
    check_workflow_access(
        workflow,
        command,
        allow_docker,
        runtime.capabilities().read_only_workspace(),
        runtime.display_name(),
    )
}

pub(crate) fn check_workflow_access(
    workflow: &Workflow,
    command: WorkspaceAccess,
    allow_docker: bool,
    runtime_supports_read_only: bool,
    runtime_name: &str,
) -> Result<(), CommandError> {
    if command.is_read_only() && (!workflow.setup.is_empty() || !workflow.teardown.is_empty()) {
        return Err(CommandError::InvalidFlagValue {
            command: Vec::new(),
            flag: READ_ONLY_WORKSPACE_FLAG.to_string(),
            reason: SETUP_TEARDOWN_REFUSAL.to_string(),
        });
    }
    let strictest = workflow
        .steps
        .iter()
        .fold(command, |access, step| step.workspace_access(access));
    check_workspace_access(
        strictest,
        allow_docker,
        runtime_supports_read_only,
        runtime_name,
    )
}

/// One agent step's route, as the `--plan` workflow preflight sees it.
#[derive(Debug, Clone)]
pub struct NativeModeStep {
    pub step: String,
    pub agent: crate::data::session::AgentName,
    pub launch_mode: crate::data::config::agents::LaunchMode,
    pub non_interactive: bool,
    pub model_requested: bool,
}

/// The workflow-wide native mode preflight of `exec workflow --plan` (WI
/// 0070): every agent step's agent must be able to deliver `--plan` on its
/// own route. Fails before setup, any worktree or any container. A no-op
/// when `plan` is false. Independent of the mount preflight above.
pub fn preflight_workflow_native_mode(
    steps: &[NativeModeStep],
    plan: bool,
    session: &crate::data::session::Session,
    paradigm: crate::engine::agent::definition::RunParadigm,
) -> Result<(), CommandError> {
    if !plan {
        return Ok(());
    }
    for step in steps {
        let def = crate::engine::agent::AgentRegistry::resolve_for_session(session, &step.agent)?;
        let probe = crate::engine::agent::AgentRunOptions {
            plan: Some(crate::engine::container::options::PlanMode::Enabled),
            non_interactive: step.non_interactive,
            model: step.model_requested.then(String::new),
            launch_mode: step.launch_mode,
            ..Default::default()
        };
        // The typed refusal is kept (its code is the client's to act on);
        // a route refusal also names the step.
        def.native_plan_delivery(&probe, paradigm)
            .map_err(|e| match e {
                crate::engine::error::EngineError::NativePlanRouteUnsupported {
                    agent,
                    route,
                    reason,
                } => crate::engine::error::EngineError::NativePlanRouteUnsupported {
                    agent,
                    route,
                    reason: format!("workflow step '{}': {reason}", step.step),
                },
                other => other,
            })
            .map_err(CommandError::from)?;
    }
    Ok(())
}

/// The flag-level rule of `exec workflow`: `--read-only-workspace` and
/// `--dynamic` are mutually exclusive. Checked by the command, not the
/// catalogue, because the flag spec is shared with `exec prompt`, which has
/// no `--dynamic`.
pub fn check_workflow_flags(access: WorkspaceAccess, dynamic: bool) -> Result<(), CommandError> {
    if access.is_read_only() && dynamic {
        return Err(CommandError::InvalidFlagValue {
            command: vec!["exec".into(), "workflow".into()],
            flag: READ_ONLY_WORKSPACE_FLAG.to_string(),
            reason: DYNAMIC_REASON.to_string(),
        });
    }
    Ok(())
}

/// The access a launch really gets: the requested access, made read-only if
/// `engine` is already read-only (a pulled lease with `exec.read_only`).
pub fn effective_access(requested: WorkspaceAccess, engine: &AgentEngine) -> WorkspaceAccess {
    requested.strictest(WorkspaceAccess::from_read_only(
        engine.read_only_workspace(),
    ))
}

/// The agent engine to launch with: a read-only clone when `access` asks.
/// Never clears read-only: an engine that is already read-only is returned
/// as it is whatever `access` says.
pub fn agent_engine_for(access: WorkspaceAccess, engine: &Arc<AgentEngine>) -> Arc<AgentEngine> {
    if !access.is_read_only() || engine.read_only_workspace() {
        return Arc::clone(engine);
    }
    Arc::new((**engine).clone().with_read_only_workspace(true))
}

/// Make a mount that is part of the workspace (a linked worktree's Git
/// directory) read-only when `access` is read-only. Never widens a mount.
pub fn restrict_to_access(
    mount: &mut crate::engine::overlay::DirectorySpec,
    access: WorkspaceAccess,
) {
    if access.is_read_only() {
        mount.permission = OverlayPermission::ReadOnly;
    }
}

/// The guest paths of the context directories mounted writable, in mount
/// order — the "mounted contexts" the prompt line names.
pub fn writable_contexts(context_overlays: &[ContextOverlay]) -> Vec<String> {
    context_overlays
        .iter()
        .filter(|overlay| overlay.permission == OverlayPermission::ReadWrite)
        .map(|overlay| overlay.container_path.display().to_string())
        .collect()
}

/// Apply the read-only prompt policy: append the `## Read-only workspace`
/// section (after every other section, including the outputs section) when
/// `access` is read-only; leave the prompt untouched otherwise.
pub fn system_prompt_for(
    access: WorkspaceAccess,
    system_prompt: Option<String>,
    outputs_mounted: bool,
    context_overlays: &[ContextOverlay],
) -> Option<String> {
    if !access.is_read_only() {
        return system_prompt;
    }
    Some(crate::data::workspace::with_read_only_workspace_section(
        system_prompt,
        outputs_mounted,
        &writable_contexts(context_overlays),
    ))
}

/// The one `Info` message a read-only launch reports. `step` is `None` for
/// `exec prompt` and the step name for a workflow step.
pub fn read_only_message(step: Option<&str>) -> UserMessage {
    let text = match step {
        None => {
            "Workspace mounted read-only; writable: /awman/output, mounted contexts, /tmp."
                .to_string()
        }
        Some(name) => format!(
            "Step '{name}': workspace mounted read-only; writable: /awman/output, mounted contexts, /tmp."
        ),
    };
    UserMessage {
        level: MessageLevel::Info,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::engine::overlay::ContextScope;
    use WorkspaceAccess::{ReadOnly, ReadWrite};

    fn context(path: &str, permission: OverlayPermission) -> ContextOverlay {
        ContextOverlay {
            scope: ContextScope::Repo,
            host_path: PathBuf::from("/host"),
            container_path: PathBuf::from(path),
            permission,
        }
    }

    fn workflow(toml: &str) -> Workflow {
        Workflow::parse(toml, crate::data::workflow_definition::WorkflowFormat::Toml).unwrap()
    }

    #[test]
    fn read_write_always_passes_preflight() {
        check_workspace_access(ReadWrite, true, false, "Docker Sandboxes").unwrap();
    }

    #[test]
    fn read_only_is_refused_on_a_runtime_that_cannot_enforce_it() {
        let err = check_workspace_access(ReadOnly, false, false, "Docker Sandboxes").unwrap_err();
        assert_eq!(
            err.to_string(),
            "the Docker Sandboxes runtime cannot mount the workspace read-only; \
             --read-only-workspace is refused rather than run with a writable workspace"
        );
        assert_eq!(err.code(), "read_only_workspace_unsupported");
        assert_eq!(
            err.stage(),
            crate::command::error::failure_stages::PREFLIGHT
        );
        assert_eq!(err.exit_code(), 2);
        check_workspace_access(ReadOnly, false, true, "Docker").unwrap();
    }

    #[test]
    fn read_only_with_allow_docker_is_refused_even_where_supported() {
        let err = check_workspace_access(ReadOnly, true, true, "Docker").unwrap_err();
        match err {
            CommandError::InvalidFlagValue { flag, reason, .. } => {
                assert_eq!(flag, "read-only-workspace");
                assert_eq!(reason, ALLOW_DOCKER_REASON);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn dynamic_is_refused_only_with_read_only() {
        assert!(check_workflow_flags(ReadOnly, true).is_err());
        check_workflow_flags(ReadOnly, false).unwrap();
        check_workflow_flags(ReadWrite, true).unwrap();
    }

    #[test]
    fn one_read_only_step_makes_the_whole_workflow_preflight_strict() {
        let wf = workflow(
            "[[step]]\nname = \"a\"\nprompt = \"x\"\n\n\
             [[step]]\nname = \"b\"\nprompt = \"y\"\nread_only_workspace = true\n",
        );
        let err =
            check_workflow_access(&wf, ReadWrite, false, false, "Docker Sandboxes").unwrap_err();
        assert!(matches!(
            err,
            CommandError::ReadOnlyWorkspaceUnsupported { .. }
        ));
        check_workflow_access(&wf, ReadWrite, false, true, "Docker").unwrap();
        // No read-only step and no flag: nothing to check on any runtime.
        let plain = workflow("[[step]]\nname = \"a\"\nprompt = \"x\"\n");
        check_workflow_access(&plain, ReadWrite, true, false, "Docker Sandboxes").unwrap();
    }

    #[test]
    fn command_flag_refuses_setup_and_teardown_but_the_step_field_does_not() {
        let wf = workflow(
            "[[step]]\nname = \"a\"\nprompt = \"x\"\nread_only_workspace = true\n\n\
             [[setup]]\ntype = \"checkout_create_branch\"\nbranch = \"x\"\n",
        );
        assert_eq!(wf.setup.len(), 1);
        let err = check_workflow_access(&wf, ReadOnly, false, true, "Docker").unwrap_err();
        assert!(err.to_string().contains(SETUP_TEARDOWN_REFUSAL), "{err}");
        check_workflow_access(&wf, ReadWrite, false, true, "Docker").unwrap();
    }

    #[test]
    fn agent_engine_for_never_clears_read_only() {
        let engine = Arc::new(test_engine());
        assert!(Arc::ptr_eq(&agent_engine_for(ReadWrite, &engine), &engine));
        let ro = agent_engine_for(ReadOnly, &engine);
        assert!(ro.read_only_workspace());
        assert!(
            !engine.read_only_workspace(),
            "the shared engine is not mutated"
        );
        // A lease engine that is already read-only stays read-only.
        let lease = Arc::new(test_engine().with_read_only_workspace(true));
        assert!(agent_engine_for(ReadWrite, &lease).read_only_workspace());
        assert_eq!(effective_access(ReadWrite, &lease), ReadOnly);
        assert_eq!(effective_access(ReadWrite, &engine), ReadWrite);
        assert_eq!(effective_access(ReadOnly, &engine), ReadOnly);
    }

    fn test_engine() -> AgentEngine {
        // Never launched: only the read-only switch is inspected. The
        // resolver points at a path that is never read.
        let runtime: Arc<dyn AgentRuntimeEngine> =
            Arc::new(crate::engine::container::ContainerRuntime::docker_in(
                crate::data::config::env::EnvSnapshot::empty(),
            ));
        AgentEngine::new(
            Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
                crate::data::fs::AuthPathResolver::at_home("/nonexistent-awman-test-home"),
            )),
            runtime,
        )
    }

    #[test]
    fn prompt_section_is_added_only_when_read_only_and_after_existing_sections() {
        let contexts = [
            context("/awman/context/repo", OverlayPermission::ReadWrite),
            context("/awman/context/docs", OverlayPermission::ReadOnly),
            context("/awman/context/workflow", OverlayPermission::ReadWrite),
        ];
        let existing = Some("## Outputs\n\nwrite to /awman/output".to_string());
        assert_eq!(
            system_prompt_for(ReadWrite, existing.clone(), true, &contexts),
            existing
        );
        let prompt = system_prompt_for(ReadOnly, existing, true, &contexts).unwrap();
        let outputs_at = prompt.find("## Outputs").unwrap();
        let ro_at = prompt.find("## Read-only workspace").unwrap();
        assert!(outputs_at < ro_at, "{prompt}");
        assert_eq!(prompt.matches("## Read-only workspace").count(), 1);
        assert!(prompt.ends_with(
            "Write anything that must outlive this run to /awman/output or to the mounted \
             context directories (/awman/context/repo, /awman/context/workflow). Use /tmp for \
             scratch files; it is discarded when the run ends."
        ));
        assert!(!prompt.contains("/awman/context/docs"));
        // No prompt at all still gets the section.
        let alone = system_prompt_for(ReadOnly, None, false, &[]).unwrap();
        assert!(alone.starts_with("## Read-only workspace"));
        assert!(alone.contains("No durable writable directory is mounted"));
    }

    #[test]
    fn info_messages_are_exact() {
        assert_eq!(
            read_only_message(None).text,
            "Workspace mounted read-only; writable: /awman/output, mounted contexts, /tmp."
        );
        let step = read_only_message(Some("review"));
        assert_eq!(step.level, MessageLevel::Info);
        assert_eq!(
            step.text,
            "Step 'review': workspace mounted read-only; writable: /awman/output, mounted contexts, /tmp."
        );
    }
}
