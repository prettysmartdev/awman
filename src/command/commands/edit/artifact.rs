//! Shared runner for the three file-artifact kinds: workflow, skill and agent.
//!
//! Order of operations (design §8.2): resolve the typed target (no launch on
//! failure, nothing is ever scaffolded) → forward notices → resolve the agent
//! and its credentials once → stage global/library bundles into the repo
//! context → render the edit prompt → run the agent → check in place or
//! publish the stage → `conclude`.
//!
//! Trust: nothing here records an approval. A changed agent bundle only gets a
//! re-approval notice; its new hash is checked by the normal trust gate the
//! next time it is launched. The agent that runs this session is resolved (and
//! its options built, with their trust check) before the agent starts, so
//! editing that same definition cannot change the running session.

use std::path::PathBuf;

use super::*;
use crate::command::commands::launch_policy::agent_session::{
    resolve_agent, resolve_credentials, run_agent_session, AgentSessionError, AgentSessionRequest,
};
use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::prompt_templates::edit::{
    render_edit_prompt, EditLocation, EditPromptRequest, EditSubject,
};
use crate::command::work_item::edit::{
    reject_explicit_read_only_repo_context, require_writable_repo_context,
};
use crate::data::fs::edit_stage::{stage_paths, EditStage};
use crate::data::fs::edit_target::{
    check_in_place, resolve_artifact_target, ArtifactFileRole, ArtifactKind, ArtifactRequest,
    ArtifactScope, ArtifactTarget,
};
use crate::data::WorkflowFormat;

/// Run `edit workflow|skill|agent` for one resolved kind.
pub(crate) async fn run(
    command: &EditCommand,
    kind: ArtifactKind,
    flags: EditArtifactFlags,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<EditOutcome, CommandError> {
    let edit_kind = edit_kind(kind);
    let creation_label = format!("new {}", edit_kind.as_str());
    let label = if command.creating {
        &creation_label
    } else {
        edit_kind.label()
    };
    let engines = command.engines();
    let session = command.session();
    let common = &flags.common;

    // 1. Resolve the typed target. Lookup only; nothing is created.
    let target = resolve_artifact_target(&ArtifactRequest {
        kind,
        reference: &common.reference,
        global: flags.global,
        env: session.env(),
        git_root: session.git_root(),
        definition_root: session.definition_root(),
        cwd: session.working_dir(),
    })
    .map_err(|e| {
        let text = e.to_string();
        error(frontend, label, &text);
        CommandError::Edit(EditError::Target(text))
    })?;

    // 2. Forward what resolution found.
    for notice in &target.notices {
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: format!("{label}: {notice}"),
        });
    }
    if !target.initial_problems.is_empty() {
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: format!(
                "{label}: {} '{}' currently has problems (it can still be edited to repair them):{}",
                kind.as_str(),
                target.name,
                bullet_list(&target.initial_problems)
            ),
        });
    }

    // Overlay flags are parsed before anything is staged or launched.
    let cli_overlays = LaunchPolicy::parse_overlay_flags(&common.overlays).inspect_err(|e| {
        error(frontend, label, &format!("invalid overlay spec: {e}"));
    })?;
    // Staging needs a writable repo context: refuse an explicit read-only
    // request before the collector can discard it, and before staging.
    if !target.edits_in_place() {
        reject_explicit_read_only_repo_context(&cli_overlays).map_err(|e| {
            error(frontend, label, &e.to_string());
            CommandError::WorkItem(e)
        })?;
    } else if engines.agent_engine.read_only_workspace() {
        let text = format!(
            "a repository {} cannot be edited through a read-only repository mount",
            kind.as_str()
        );
        error(frontend, label, &text);
        return Err(CommandError::Edit(EditError::Target(text)));
    }
    require_repository_mount(session, frontend, label, &format!("a {}", kind.as_str()))?;

    // 3. Agent and credentials, once, before staging (design §4.7).
    let agent = resolve_agent(session, &common.agent, label, frontend)?;
    let credentials = resolve_credentials(engines, session, &agent, label, frontend)?;
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "{label}: editing {} '{}' ({}) with agent '{}'",
            kind.as_str(),
            target.name,
            scope_text(&target.scope),
            agent
        ),
    });

    let mut collected = session
        .effective_config()
        .collected_overlays(cli_overlays, None, None)?;

    // 4. Staged only: a private copy under the writable repo context.
    let stage = if target.edits_in_place() {
        None
    } else {
        let git_root = session.git_root().to_path_buf();
        let origin_url = engines.git_engine.remote_effective_url(&git_root, "origin");
        let ctx = require_writable_repo_context(session, origin_url.as_deref(), &mut collected)
            .map_err(|e| {
                error(frontend, label, &e.to_string());
                CommandError::WorkItem(e)
            })?;
        let stage = EditStage::create(&target, stage_paths(&ctx.host_dir, uuid::Uuid::new_v4()))
            .map_err(|e| {
                error(
                    frontend,
                    label,
                    &format!("could not stage the {}: {e}", kind.as_str()),
                );
                CommandError::from(e)
            })?;
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("{label}: draft at {}", stage.paths().host_dir.display()),
        });
        Some(stage)
    };

    // 5. Context overlays, target, prompt and run options.
    let resolved = LaunchPolicy::for_session(session)
        .with_git(&engines.git_engine)
        .resolve_context_overlays(
            &collected.context_overlays,
            &agent,
            None,
            None,
            &mut *frontend,
        );
    let (context_overlays, system_prompt) = match resolved {
        Ok(v) => v,
        Err(e) => return Err(discard(stage, frontend, label, e)),
    };
    let edit_target = build_edit_target(edit_kind, &target, stage.as_ref());
    let prompt = render_prompt(&target, &edit_target, common, command.creating);

    let mut run_opts = run_options(common);
    run_opts.command_secrets = engines.command_secrets();
    run_opts.initial_prompt = Some(prompt);
    run_opts.context_overlays = context_overlays;
    run_opts.system_prompt = system_prompt;
    run_opts.directory_overlays = collected.directories;
    run_opts.include_all_skills = collected.include_all_skills;
    run_opts.named_skills = collected.named_skills;
    run_opts.env_passthrough =
        (!collected.env_passthrough.is_empty()).then_some(collected.env_passthrough);

    let execution = run_agent_session(
        engines,
        session,
        AgentSessionRequest {
            label,
            agent: &agent,
            run_opts,
            credentials: Some(credentials),
        },
        command.cancel(),
        &mut *frontend,
    )
    .await;

    // 6. Launch failure: nothing ran, originals untouched, draft removed.
    let execution: Result<i32, String> = match execution {
        Ok(info) => Ok(info.exit_code),
        Err(AgentSessionError::Wait(e)) => Err(format!("failed to wait for the agent: {e}")),
        Err(AgentSessionError::Launch(e)) => return Err(discard(stage, frontend, label, e)),
    };

    // 7. Save: always, whatever the exit code (cancellation is not rollback).
    let mut save = match stage {
        Some(stage) => publish(stage),
        None => check(&target),
    };

    // 8. Changed agent bundles must be approved again before their next use.
    if let Ok(summary) = &mut save {
        if kind == ArtifactKind::Agent && !summary.changed.is_empty() {
            summary.notices.push(reapproval_notice(&target.name));
        }
    }
    if let Err(failure) = &save {
        if kind == ArtifactKind::Agent && !failure.published.is_empty() {
            warn(frontend, label, &reapproval_notice(&target.name));
        }
    }
    // `conclude` drops notices when the session failed; show them here so a
    // failed run still says what changed and what must be re-approved.
    let succeeded = matches!(execution, Ok(0)) && save.is_ok();
    if !succeeded {
        if let Ok(summary) = &save {
            for notice in &summary.notices {
                warn(frontend, label, notice);
            }
        }
    }

    // 9. One shared rule for reporting both failures.
    conclude(label, edit_target, common, execution, save, frontend)
}

fn edit_kind(kind: ArtifactKind) -> EditKind {
    match kind {
        ArtifactKind::Workflow => EditKind::Workflow,
        ArtifactKind::Skill => EditKind::Skill,
        ArtifactKind::Agent => EditKind::Agent,
    }
}

fn edit_scope(scope: &ArtifactScope) -> EditScope {
    match scope {
        ArtifactScope::Repo => EditScope::Repo,
        ArtifactScope::Global => EditScope::Global,
        ArtifactScope::Library { slug } => EditScope::Library { slug: slug.clone() },
    }
}

fn scope_text(scope: &ArtifactScope) -> String {
    match scope {
        ArtifactScope::Repo => "repository".into(),
        ArtifactScope::Global => "global".into(),
        ArtifactScope::Library { slug } => format!("library '{slug}'"),
    }
}

fn edit_role(role: ArtifactFileRole) -> EditFileRole {
    match role {
        ArtifactFileRole::Primary => EditFileRole::Primary,
        ArtifactFileRole::Companion => EditFileRole::Companion,
        ArtifactFileRole::Supporting => EditFileRole::Supporting,
    }
}

/// The plain-data target: host paths are the originals, container paths what
/// the agent opens (the checkout, or the stage under `/awman/context/repo`).
fn build_edit_target(
    kind: EditKind,
    target: &ArtifactTarget,
    stage: Option<&EditStage>,
) -> EditTarget {
    let container_path = |rel: &str| -> String {
        match stage {
            Some(stage) => stage.container_path(rel),
            None => workspace_path(target, rel),
        }
    };
    let files = target
        .files
        .iter()
        .map(|file| EditFile {
            host_path: target.host_path(file),
            container_path: container_path(&file.rel),
            role: edit_role(file.role),
        })
        .collect();
    let bundle_dir = target.directory_bundle.then(|| match stage {
        Some(stage) => stage.paths().container_dir.clone(),
        None => workspace_path(target, ""),
    });
    EditTarget {
        kind,
        identity: target.name.clone(),
        scope: edit_scope(&target.scope),
        persistence: match stage {
            Some(stage) => EditPersistence::Staged {
                stage_dir: stage.paths().host_dir.clone(),
            },
            None => EditPersistence::InPlace,
        },
        files,
        bundle_dir,
    }
}

/// `/workspace/<workspace_rel>/<rel>` with empty parts dropped.
fn workspace_path(target: &ArtifactTarget, rel: &str) -> String {
    let mut path = String::from("/workspace");
    for part in [target.workspace_rel.as_deref().unwrap_or(""), rel] {
        if !part.is_empty() {
            path.push('/');
            path.push_str(part);
        }
    }
    path
}

fn render_prompt(
    target: &ArtifactTarget,
    edit: &EditTarget,
    flags: &EditCommonFlags,
    creating: bool,
) -> String {
    let format = match target.format {
        Some(WorkflowFormat::Yaml) => "YAML",
        _ => "TOML",
    };
    let subject = match target.kind {
        ArtifactKind::Workflow => EditSubject::Workflow { format },
        ArtifactKind::Skill => EditSubject::Skill,
        ArtifactKind::Agent => EditSubject::Agent,
    };
    let primary = edit
        .files
        .iter()
        .find(|f| f.role == EditFileRole::Primary)
        .or(edit.files.first())
        .map(|f| f.container_path.clone())
        .unwrap_or_default();
    // Skills name their directory instead; agents list the Dockerfile.
    let companions: Vec<String> = edit
        .files
        .iter()
        .filter(|f| f.role == EditFileRole::Companion)
        .map(|f| f.container_path.clone())
        .collect();
    render_edit_prompt(&EditPromptRequest {
        creating,
        subject,
        name: &target.name,
        primary_path: &primary,
        companion_paths: &companions,
        bundle_dir: edit.bundle_dir.as_deref(),
        location: if target.edits_in_place() {
            EditLocation::InPlace
        } else {
            EditLocation::StagedCopy
        },
        global: target.scope != ArtifactScope::Repo,
        amend: flags.amend,
        headless: flags.headless,
        brief: flags.prompt.as_deref(),
    })
}

/// Staged: validate and publish back to the original location.
fn publish(stage: EditStage) -> Result<SaveSummary, SaveFailure> {
    match stage.publish() {
        Ok(report) => Ok(SaveSummary {
            changed: report
                .published
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            notices: report
                .ignored
                .iter()
                .map(|entry| format!("not saved: {entry}"))
                .collect(),
        }),
        Err(failure) => Err(SaveFailure {
            message: failure.to_string(),
            retained: vec![failure.stage_dir.clone()],
            published: failure.published.clone(),
        }),
    }
}

/// In place: the agent already wrote the files; re-read and validate them.
/// Invalid edits stay on disk as the agent left them (no rollback).
fn check(target: &ArtifactTarget) -> Result<SaveSummary, SaveFailure> {
    let report = check_in_place(target).map_err(|e| SaveFailure {
        message: format!("could not re-read the edited files: {e}"),
        retained: Vec::new(),
        published: Vec::new(),
    })?;
    let changed: Vec<String> = report
        .changed
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    if report.problems.is_empty() {
        return Ok(SaveSummary {
            changed,
            notices: Vec::new(),
        });
    }
    if report.changed.is_empty() {
        // Nothing was edited: a no-op is success, but the artifact is still broken.
        return Ok(SaveSummary {
            changed,
            notices: vec![format!(
                "{} '{}' still has problems:{}",
                target.kind.as_str(),
                target.name,
                bullet_list(&report.problems)
            )],
        });
    }
    Err(SaveFailure {
        message: format!(
            "the edited {} is invalid; the changes were left in place for you to fix:{}\nchanged files:{}",
            target.kind.as_str(),
            bullet_list(&report.problems),
            bullet_list(&changed)
        ),
        retained: report.changed.clone(),
        published: report.changed,
    })
}

/// The agent never ran: remove the unused draft and return `error`.
fn discard(
    stage: Option<EditStage>,
    frontend: &mut dyn AgentLaunchFrontend,
    label: &str,
    error: CommandError,
) -> CommandError {
    if let Some(stage) = stage {
        let dir: PathBuf = stage.paths().host_dir.clone();
        if let Err(e) = stage.discard() {
            warn(
                frontend,
                label,
                &format!(
                    "could not remove the unused draft at {}: {e}",
                    dir.display()
                ),
            );
        }
    }
    error
}

fn reapproval_notice(name: &str) -> String {
    format!("`{name}` changed; review and approve it again with `awman ready --agent {name}`")
}

fn bullet_list(items: &[String]) -> String {
    items.iter().map(|i| format!("\n  - {i}")).collect()
}

fn error(frontend: &mut dyn AgentLaunchFrontend, label: &str, text: &str) {
    frontend.write_message(UserMessage {
        level: MessageLevel::Error,
        text: format!("{label}: {text}"),
    });
}

fn warn(frontend: &mut dyn AgentLaunchFrontend, label: &str, text: &str) {
    frontend.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: format!("{label}: {text}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fs::edit_target::ArtifactFile;

    fn file(rel: &str, role: ArtifactFileRole) -> ArtifactFile {
        ArtifactFile {
            rel: rel.into(),
            role,
            sha256: String::new(),
            len: 0,
        }
    }

    fn target(
        kind: ArtifactKind,
        scope: ArtifactScope,
        workspace_rel: Option<&str>,
    ) -> ArtifactTarget {
        let files = match kind {
            ArtifactKind::Workflow => vec![file("release.yaml", ArtifactFileRole::Primary)],
            ArtifactKind::Skill => vec![
                file("SKILL.md", ArtifactFileRole::Primary),
                file("refs/a.md", ArtifactFileRole::Supporting),
            ],
            ArtifactKind::Agent => vec![
                file("bot.toml", ArtifactFileRole::Primary),
                file("Dockerfile.bot", ArtifactFileRole::Companion),
            ],
        };
        ArtifactTarget {
            kind,
            name: "release".into(),
            scope,
            root: PathBuf::from("/repo/sub"),
            files,
            directory_bundle: kind == ArtifactKind::Skill,
            format: (kind == ArtifactKind::Workflow).then_some(WorkflowFormat::Yaml),
            workspace_rel: workspace_rel.map(str::to_string),
            initial_problems: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn flags() -> EditCommonFlags {
        EditCommonFlags {
            reference: "release".into(),
            amend: false,
            headless: true,
            prompt: Some("rename step {path}".into()),
            json: false,
            allow_docker: false,
            yolo: false,
            yolo_authorized: false,
            auto: false,
            agent: None,
            model: None,
            overlays: Vec::new(),
            agent_flags: None,
        }
    }

    #[test]
    fn in_place_targets_use_workspace_paths_and_keep_original_host_paths() {
        let t = target(
            ArtifactKind::Skill,
            ArtifactScope::Repo,
            Some(".claude/skills/x"),
        );
        let edit = build_edit_target(EditKind::Skill, &t, None);
        assert_eq!(edit.persistence, EditPersistence::InPlace);
        assert_eq!(edit.scope, EditScope::Repo);
        assert_eq!(
            edit.files[0].container_path,
            "/workspace/.claude/skills/x/SKILL.md"
        );
        assert_eq!(edit.files[0].host_path, PathBuf::from("/repo/sub/SKILL.md"));
        assert_eq!(edit.files[1].role, EditFileRole::Supporting);
        assert_eq!(
            edit.bundle_dir.as_deref(),
            Some("/workspace/.claude/skills/x")
        );
    }

    #[test]
    fn repo_root_workspace_rel_has_no_double_slash() {
        let t = target(ArtifactKind::Workflow, ArtifactScope::Repo, Some(""));
        let edit = build_edit_target(EditKind::Workflow, &t, None);
        assert_eq!(edit.files[0].container_path, "/workspace/release.yaml");
        assert_eq!(edit.bundle_dir, None);
    }

    #[test]
    fn prompt_preserves_format_names_companions_and_keeps_brief_verbatim() {
        let t = target(
            ArtifactKind::Workflow,
            ArtifactScope::Repo,
            Some("aspec/workflows"),
        );
        let edit = build_edit_target(EditKind::Workflow, &t, None);
        let prompt = render_prompt(&t, &edit, &flags(), false);
        assert!(prompt.contains("/workspace/aspec/workflows/release.yaml"));
        assert!(prompt.contains("YAML"));
        assert!(prompt.contains("rename step {path}"));

        let a = target(ArtifactKind::Agent, ArtifactScope::Global, Some("x"));
        let edit = build_edit_target(EditKind::Agent, &a, None);
        assert_eq!(edit.scope, EditScope::Global);
        let prompt = render_prompt(&a, &edit, &flags(), false);
        assert!(prompt.contains("/workspace/x/Dockerfile.bot"));
    }

    #[test]
    fn library_scope_maps_to_library_edit_scope() {
        assert_eq!(
            edit_scope(&ArtifactScope::Library {
                slug: "acme".into()
            }),
            EditScope::Library {
                slug: "acme".into()
            }
        );
        assert_eq!(edit_kind(ArtifactKind::Agent).label(), "edit agent");
    }

    #[test]
    fn reapproval_notice_names_the_ready_command() {
        assert_eq!(
            reapproval_notice("bot"),
            "`bot` changed; review and approve it again with `awman ready --agent bot`"
        );
    }
}
