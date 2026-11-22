//! Shared spec creation, work-item edit resolution and remote draft sessions.
//! Frontends only supply questions and launch I/O; source selection and saving
//! stay in these command-layer services.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::draft_sync::{
    DraftSync, DraftSyncError, DraftSyncMode, DraftSyncRequest, FinalDraftSyncOutcome,
};
use super::edit::{prepare_draft_edit, wait_with_draft_sync};
use super::{
    ResolvedWorkItem, WorkItemCreationTarget, WorkItemError, WorkItemOrigin, WorkItemResolver,
};
use crate::command::commands::agent_setup::AgentLaunchFrontend;
use crate::command::commands::launch_policy::agent_session::{
    build_instance, resolve_agent, resolve_credentials, run_agent_session, AgentSessionError,
    AgentSessionRequest,
};
use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::prompt_templates::render_new_spec_prompt;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::issue::Issue;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::AgentExitInfo;
use crate::engine::auth::AgentCredentials;
use crate::engine::error::EngineError;
use crate::engine::issue::IssueSource;

/// What `create_new_spec` produced: a local file (`created_path`), a remote
/// work item (`created_url`, plus the provider's `number` when it has one),
/// or nothing (an interview for a remote source that was left unedited).
#[derive(Debug, Clone, Serialize)]
pub struct NewSpecOutcome {
    pub created_path: Option<String>,
    pub created_url: Option<String>,
    pub number: Option<u32>,
}

/// Work-item kinds accepted by the spec commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum WorkItemKind {
    Feature,
    Bug,
    Task,
    Enhancement,
}

impl WorkItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkItemKind::Feature => "Feature",
            WorkItemKind::Bug => "Bug",
            WorkItemKind::Task => "Task",
            WorkItemKind::Enhancement => "Enhancement",
        }
    }
}

/// Questions used only by spec creation. Editing needs `AgentLaunchFrontend`.
pub trait NewSpecFrontend: UserMessageSink + AgentLaunchFrontend + Send + Sync {
    /// Prompt the user for the title of the new spec. Returns the title text.
    /// A frontend without an input surface must refuse to invent an answer.
    fn ask_spec_title(&mut self) -> Result<String, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "spec title".into(),
        })
    }

    /// Prompt for a one-line summary.
    fn ask_spec_summary(&mut self) -> Result<String, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "spec summary".into(),
        })
    }

    /// Put `prompt` to the user and return the kind they chose.
    ///
    /// The copy is `command::prompts::work_item_kind()`'s (WI 0114 F-19); a
    /// frontend renders the choices, maps a keypress or an index back through
    /// `Prompt::answer_for_key` / `answer_at`, and returns the result. The
    /// default is the safe-default-on-pipe behaviour expected of every Q&A
    /// method.
    fn ask_spec_kind(
        &mut self,
        _prompt: &Prompt<WorkItemKind>,
    ) -> Result<WorkItemKind, CommandError> {
        Ok(WorkItemKind::Task)
    }
}

/// An existing work item, resolved before launch. Remote draft seeds must use
/// the raw `issue.body`; the title is supplied separately to the prompt.
pub enum SpecEditTarget {
    Local {
        item: ResolvedWorkItem,
        host_path: PathBuf,
        container_path: String,
    },
    Remote {
        item: ResolvedWorkItem,
        issue: Issue,
        source: Arc<dyn IssueSource>,
        url: String,
    },
}

impl SpecEditTarget {
    pub fn item(&self) -> &ResolvedWorkItem {
        match self {
            Self::Local { item, .. } | Self::Remote { item, .. } => item,
        }
    }
}

/// Use the full shared reference grammar and the resolved item's own provider.
/// Errors are reported once and never cause probing of another source.
pub async fn resolve_spec_edit_target(
    engines: &Engines,
    session: &Session,
    reference: &str,
    label: &str,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<SpecEditTarget, CommandError> {
    let resolver = WorkItemResolver::new(session.clone(), Arc::clone(&engines.git_engine));
    let item = resolver
        .resolve(Some(reference), frontend)
        .await
        .map_err(|e| report(frontend, label, e))?
        .ok_or_else(|| report(frontend, label, WorkItemError::MissingInput))?;
    match &item.origin {
        WorkItemOrigin::Local { path } => {
            let host_path = path.clone();
            let container_path = Path::new("/workspace")
                .join(super::resolver::repo_relative_display(
                    session.git_root(),
                    path,
                ))
                .display()
                .to_string();
            Ok(SpecEditTarget::Local {
                item,
                host_path,
                container_path,
            })
        }
        WorkItemOrigin::Remote { .. } => {
            let source = resolver
                .source_for(&item)
                .map_err(|e| report(frontend, label, e))?;
            let issue = item
                .issue
                .clone()
                .ok_or_else(|| report(frontend, label, WorkItemError::InlineNotEditable))?;
            let url = crate::data::issue::display_url(&issue.source_id);
            Ok(SpecEditTarget::Remote {
                item,
                issue,
                source,
                url,
            })
        }
    }
}

pub struct RemoteSpecEditEnd {
    pub execution: Result<AgentExitInfo, EngineError>,
    pub sync: Result<FinalDraftSyncOutcome, DraftSyncError>,
    pub draft_host_path: PathBuf,
}

/// `new spec` body. Resolves the creation target and the template, runs the
/// title/type/summary questions through the supplied frontend, then hands a
/// local file or a remote draft to an agent for completion.
/// Called by `NewCommand::Spec`.
pub(crate) async fn create_new_spec(
    engines: &Engines,
    session: Session,
    flags: crate::command::commands::new::NewSpecFlags,
    cancel: &CancellationToken,
    frontend: &mut dyn NewSpecFrontend,
) -> Result<NewSpecOutcome, CommandError> {
    const LABEL: &str = "new spec";
    let non_interactive = flags.non_interactive;
    engines.require_container_runtime()?;
    crate::command::commands::edit::require_repository_mount(&session, frontend, LABEL, "a spec")?;
    let git_root = session.git_root().to_path_buf();
    let config = session.effective_config();
    let template_path = config.work_items_template(&git_root);

    if !template_path.exists() {
        frontend.write_message(UserMessage {
            level: MessageLevel::Error,
            text: format!(
                "{LABEL}: spec template missing at {}",
                template_path.display()
            ),
        });
        return Err(CommandError::SpecTemplateMissing {
            path: template_path,
        });
    }
    let template = match crate::data::fs::work_item_files::read_work_item_template(
        &git_root,
        &template_path,
    ) {
        Ok(t) => t,
        Err(e) => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "{LABEL}: failed to read spec template {}: {e}",
                    template_path.display()
                ),
            });
            return Err(CommandError::Data(e));
        }
    };

    // Where the new work item goes, and the seed it is created from. Both
    // are decided before any side effect: a seed that already lives in the
    // target would be duplicated, so it is refused in favour of `edit spec`.
    let resolver = WorkItemResolver::new(session.clone(), Arc::clone(&engines.git_engine));
    let target = resolver
        .creation_target()
        .map_err(|e| report(frontend, LABEL, e))?;
    let seed: Option<ResolvedWorkItem> = match resolver
        .resolve(flags.work_item.as_deref(), &mut *frontend)
        .await
    {
        Ok(seed) => seed,
        Err(e) => return Err(report(frontend, LABEL, e)),
    };
    if let Some(seed) = &seed {
        resolver
            .ensure_new_in_target(seed, &target)
            .map_err(|e| report(frontend, LABEL, e))?;
    }

    let (kind, title, summary) = match &seed {
        Some(seed) => {
            let source_body = seed
                .issue
                .as_ref()
                .map_or(seed.content.as_str(), |issue| issue.body.as_str());
            let summary = match &flags.input.summary {
                Some(brief) => format!(
                    "Creation brief:\n{brief}\n\nSource work item \"{}\":\n{source_body}",
                    seed.title
                ),
                None => source_body.to_owned(),
            };
            (
                flags.kind.unwrap_or(WorkItemKind::Task),
                flags
                    .input
                    .title
                    .clone()
                    .unwrap_or_else(|| seed.title.clone()),
                summary,
            )
        }
        None => {
            let kind = match flags.kind {
                Some(kind) => kind,
                None if non_interactive => WorkItemKind::Task,
                None => frontend.ask_spec_kind(&crate::command::prompts::work_item_kind())?,
            };
            let title = flags
                .input
                .title(non_interactive, || frontend.ask_spec_title())?;
            let summary = flags
                .input
                .summary(non_interactive, || frontend.ask_spec_summary())?;
            (kind, title, summary)
        }
    };
    let agent = resolve_agent(&session, &None, LABEL, frontend)?;
    let credentials = resolve_credentials(engines, &session, &agent, LABEL, frontend)?;
    let mut run_opts = AgentRunOptions {
        non_interactive,
        yolo_authorized: flags.yolo_authorized,
        yolo: flags
            .yolo
            .then_some(crate::engine::container::options::YoloMode::Enabled),
        command_secrets: engines.command_secrets(),
        ..Default::default()
    };
    match target {
        WorkItemCreationTarget::Local { directory } => {
            crate::data::fs::path_guard::validate_under_root(
                &git_root,
                &directory,
                "new spec requires a work item directory inside the mounted repository",
            )?;
            if engines.agent_engine.read_only_workspace() {
                return Err(CommandError::Other(
                    "a spec cannot be created through a read-only repository mount".into(),
                ));
            }
            let number = crate::data::fs::work_item_files::next_work_item_number(&directory)
                .map_err(|e| report_data(frontend, LABEL, "failed to number the work item", e))?;
            let body = apply_work_item_template(
                &template,
                kind,
                &title,
                &summary,
                &format!("{number:04}"),
            );
            let dest = crate::data::fs::work_item_files::write_new_local_work_item(
                &git_root,
                &directory,
                number,
                &slugify(&title),
                &body,
            )
            .map_err(|e| report_data(frontend, LABEL, "failed to write work item", e))?;
            let path = Path::new("/workspace")
                .join(super::resolver::repo_relative_display(&git_root, &dest))
                .display()
                .to_string();
            let collected =
                session
                    .effective_config()
                    .collected_overlays(Vec::new(), None, None)?;
            let (context_overlays, system_prompt) = LaunchPolicy::for_session(&session)
                .with_git(&engines.git_engine)
                .resolve_context_overlays(
                    &collected.context_overlays,
                    &agent,
                    None,
                    None,
                    frontend,
                )?;
            run_opts.context_overlays = context_overlays;
            run_opts.system_prompt = system_prompt;
            run_opts.directory_overlays = collected.directories;
            run_opts.include_all_skills = collected.include_all_skills;
            run_opts.named_skills = collected.named_skills;
            run_opts.env_passthrough =
                (!collected.env_passthrough.is_empty()).then_some(collected.env_passthrough);
            run_opts.initial_prompt = Some(render_new_spec_prompt(
                &path,
                kind.as_str(),
                &title,
                &summary,
                non_interactive,
                false,
            ));
            let execution = run_agent_session(
                engines,
                &session,
                AgentSessionRequest {
                    label: LABEL,
                    agent: &agent,
                    run_opts,
                    credentials: Some(credentials),
                },
                cancel,
                frontend,
            )
            .await;
            match execution {
                Ok(info) if info.exit_code == 0 => {}
                Ok(info) => {
                    return Err(CommandError::Other(format!(
                        "new spec: agent exited with code {}; work item kept at {}",
                        info.exit_code,
                        dest.display()
                    )))
                }
                Err(AgentSessionError::Launch(error)) => return Err(error),
                Err(AgentSessionError::Wait(error)) => return Err(error.into()),
            }
            let content =
                crate::data::fs::work_item_files::read_local_work_item(&git_root, &git_root, &dest)
                    .map_err(|e| CommandError::Other(e.to_string()))?;
            if content.content.trim().is_empty() {
                return Err(CommandError::Other(format!(
                    "new spec: agent left a blank work item at {}",
                    dest.display()
                )));
            }
            Ok(NewSpecOutcome {
                created_path: Some(dest.display().to_string()),
                created_url: None,
                number: Some(number),
            })
        }
        WorkItemCreationTarget::Remote {
            provider,
            scope,
            source,
        } => {
            let body = apply_work_item_template(&template, kind, &title, &summary, "");
            frontend.write_message(UserMessage { level: MessageLevel::Info,
                text: format!("new spec: creating a {provider} work item; draft edits are published to {scope} while the agent runs"),
            });
            let created = run_remote_edit(
                engines,
                &session,
                RemoteSpecEdit {
                    agent: &agent,
                    run_opts,
                    credentials: Some(credentials),
                    source,
                    mode: DraftSyncMode::Create {
                        title: title.clone(),
                    },
                    seed: body,
                    prompt: &|path| {
                        render_new_spec_prompt(
                            path,
                            kind.as_str(),
                            &title,
                            &summary,
                            non_interactive,
                            true,
                        )
                    },
                    label: LABEL,
                },
                cancel,
                frontend,
            )
            .await?;
            if created.is_none() {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "new spec: the draft was not changed, so no work item was created".into(),
                });
            }
            Ok(NewSpecOutcome {
                created_path: None,
                number: created.as_ref().and_then(|i| i.number),
                created_url: created.map(|i| i.source_id),
            })
        }
    }
}

/// Input to a remote draft session. Existing issues always use
/// `DraftSyncMode::Update`; `Create` is reserved for `new spec` interviews.
pub struct RemoteSpecEdit<'a> {
    pub agent: &'a AgentName,
    /// The run's flags; the prompt and context overlays are filled in here.
    pub run_opts: AgentRunOptions,
    pub credentials: Option<AgentCredentials>,
    pub source: Arc<dyn IssueSource>,
    pub mode: DraftSyncMode,
    /// Initial creation content. Updates always take the raw body from the
    /// issue in `mode`, never `format_as_markdown` (which adds a title heading).
    pub seed: String,
    /// Renders the agent prompt from the draft's container path.
    pub prompt: &'a (dyn Fn(&str) -> String + Sync),
    pub label: &'a str,
}

/// Edit a remote work item through a draft: write the draft into the
/// `context(repo)` overlay (added read-write, refused if config narrows it to
/// read-only), print its host path, launch the agent, sync the draft while it
/// runs, and upload it once more after the agent exits however it exits.
///
/// `Err` means nothing ran and the unused draft was removed (or a cleanup
/// warning names it). Once the agent runs, returns execution and sync results
/// separately so the caller can report both failures. Failed or paused sync
/// retains the draft; successful sync removes it, including a no-op.
pub async fn run_remote_spec_edit(
    engines: &Engines,
    session: &Session,
    edit: RemoteSpecEdit<'_>,
    cancel: &CancellationToken,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<RemoteSpecEditEnd, CommandError> {
    run_remote_spec_edit_with_overlays(engines, session, edit, Vec::new(), cancel, frontend).await
}

/// Like `run_remote_spec_edit`, including explicit command overlays in repo
/// context selection and permission checks. Already resolved mounts alone lose
/// the context key, so edit callers pass the parsed `--overlay` values here.
pub async fn run_remote_spec_edit_with_overlays(
    engines: &Engines,
    session: &Session,
    edit: RemoteSpecEdit<'_>,
    overlays: Vec<crate::data::config::overlays::TypedOverlay>,
    cancel: &CancellationToken,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<RemoteSpecEditEnd, CommandError> {
    let RemoteSpecEdit {
        agent,
        mut run_opts,
        credentials,
        source,
        mode,
        seed,
        prompt,
        label,
    } = edit;
    // Sandbox-class runtimes: agent spawn lands in WI 0090.
    engines
        .require_container_runtime()
        .map_err(CommandError::from)?;
    let git_root = session.git_root().to_path_buf();
    let origin_url = engines.git_engine.remote_effective_url(&git_root, "origin");
    let mut collected = session
        .effective_config()
        .collected_overlays(overlays, None, None)?;
    let seed = match &mode {
        DraftSyncMode::Update { issue } => &issue.body,
        DraftSyncMode::Create { .. } => &seed,
    };
    let prepared = prepare_draft_edit(
        session,
        origin_url.as_deref(),
        &mut collected,
        seed,
        &mut *frontend,
    )
    .map_err(|e| report(frontend, label, e))?;
    let host_path = prepared.draft.paths().host_path.clone();
    let container_path = prepared.draft.paths().container_path.display().to_string();

    // Until the agent runs nothing can have edited the draft: it holds only
    // the seed (a copy of the issue body), so a failure deletes it rather
    // than leaving private issue text in the context directory.
    let discard_draft = |draft: crate::data::fs::work_item_draft::WorkItemDraft,
                         frontend: &mut dyn AgentLaunchFrontend,
                         error: CommandError| {
        if let Err(e) = draft.remove() {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "{label}: could not remove the unused draft at {}: {e}",
                    host_path.display()
                ),
            });
        }
        error
    };
    let instance = {
        let resolved = LaunchPolicy::for_session(session)
            .with_git(&engines.git_engine)
            .resolve_context_overlays(
                &collected.context_overlays,
                agent,
                None,
                None,
                &mut *frontend,
            );
        match resolved {
            Ok((context_overlays, system_prompt)) => {
                run_opts.context_overlays = context_overlays;
                run_opts.system_prompt = system_prompt;
                run_opts.directory_overlays = collected.directories;
                run_opts.include_all_skills = collected.include_all_skills;
                run_opts.named_skills = collected.named_skills;
                run_opts.env_passthrough =
                    (!collected.env_passthrough.is_empty()).then_some(collected.env_passthrough);
                run_opts.initial_prompt = Some(prompt(&container_path));
                build_instance(
                    engines,
                    session,
                    &AgentSessionRequest {
                        label,
                        agent,
                        run_opts,
                        credentials,
                    },
                    frontend,
                )
            }
            Err(e) => Err(e),
        }
    };
    let instance = match instance {
        Ok(instance) => instance,
        Err(e) => return Err(discard_draft(prepared.draft, frontend, e)),
    };
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: "Launching agent container…".into(),
    });
    frontend.set_pty_active(true);
    let cf = frontend.container_frontend_for_pty();
    let mut execution = match instance.run_with_frontend(cf) {
        Ok(e) => e,
        Err(e) => {
            frontend.set_pty_active(false);
            frontend.replay_queued();
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to run container: {e}"),
            });
            return Err(discard_draft(
                prepared.draft,
                frontend,
                CommandError::from(e),
            ));
        }
    };
    frontend.set_stuck_sender(execution.stuck_sender());
    let (running, events) = DraftSync::start(DraftSyncRequest {
        draft: prepared.draft,
        seeded_body: prepared.seeded_body,
        mode,
        source,
        git_root,
    });
    let outcome =
        wait_with_draft_sync(&mut execution, running, events, cancel, &mut *frontend).await;
    frontend.set_pty_active(false);
    frontend.wait_output_drained().await;
    frontend.replay_queued();

    Ok(RemoteSpecEditEnd {
        execution: outcome.execution,
        sync: outcome.sync,
        draft_host_path: host_path,
    })
}
/// Report sync failures before agent wait and exit failures. sync, wait, then exit status.
async fn run_remote_edit(
    engines: &Engines,
    session: &Session,
    edit: RemoteSpecEdit<'_>,
    cancel: &CancellationToken,
    frontend: &mut dyn NewSpecFrontend,
) -> Result<Option<Issue>, CommandError> {
    let label = edit.label;
    let outcome = run_remote_spec_edit(engines, session, edit, cancel, frontend).await?;
    let synced = outcome.sync.map_err(CommandError::DraftSync)?;
    match outcome.execution {
        Err(e) => Err(CommandError::from(e)),
        Ok(info) if info.exit_code != 0 => Err(CommandError::Other(format!(
            "{label}: agent exited with code {}",
            info.exit_code
        ))),
        Ok(_) => Ok(synced.issue),
    }
}

fn report(
    frontend: &mut dyn AgentLaunchFrontend,
    label: &str,
    error: WorkItemError,
) -> CommandError {
    frontend.write_message(UserMessage {
        level: MessageLevel::Error,
        text: format!("{label}: {error}"),
    });
    CommandError::WorkItem(error)
}

fn report_data(
    frontend: &mut dyn AgentLaunchFrontend,
    label: &str,
    what: &str,
    error: crate::data::error::DataError,
) -> CommandError {
    frontend.write_message(UserMessage {
        level: MessageLevel::Error,
        text: format!("{label}: {what}: {error}"),
    });
    CommandError::Data(error)
}

/// Apply all work-item substitutions to a template string.
///
/// Rules (applied in order):
/// - Lines beginning with `# Work Item:` → `# Work Item: {kind}`
/// - Lines beginning with `Title:` → `Title: {title}`
/// - `{{kind}}` token anywhere → kind string
/// - `{{number}}` token anywhere → zero-padded work item number
/// - `{{title}}` token anywhere → title
/// - `{{summary}}` token anywhere → summary text
/// - First occurrence of literal `- summary` → `- {summary}`
fn apply_work_item_template(
    template: &str,
    kind: WorkItemKind,
    title: &str,
    summary: &str,
    number: &str,
) -> String {
    let mut first_summary_replaced = false;
    let mut result = String::with_capacity(template.len() + 64);
    for line in template.lines() {
        let mut line = if line.starts_with("# Work Item:") {
            format!("# Work Item: {}", kind.as_str())
        } else if line.starts_with("Title:") {
            format!("Title: {title}")
        } else {
            line.replace("{{kind}}", kind.as_str())
                .replace("{{number}}", number)
                .replace("{{title}}", title)
                .replace("{{summary}}", summary)
        };
        if !first_summary_replaced && line.trim() == "- summary" {
            line = format!("- {summary}");
            first_summary_replaced = true;
        }
        result.push_str(&line);
        result.push('\n');
    }
    result
}

/// Slugify a title: lowercase, ASCII alphanumerics + hyphens.
pub(crate) fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".into()
    } else {
        trimmed
    }
}
