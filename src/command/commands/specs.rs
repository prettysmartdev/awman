//! `SpecsCommand` — `specs amend` — and the `new spec` body.
//!
//! Both commands work on local work item files and on remote work items
//! (WI 0147). A local work item is edited in place in the checkout. A remote
//! one is edited through a markdown draft in the `context(repo)` overlay,
//! which awman uploads to the tracker while the agent runs and once more
//! after it exits (`command::work_item::draft_sync`).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::prompt_templates::{render_amend_prompt, render_interview_prompt};
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::command::work_item::draft_sync::{DraftSync, DraftSyncMode, DraftSyncRequest};
use crate::command::work_item::edit::{prepare_draft_edit, wait_with_draft_sync};
use crate::command::work_item::{
    ResolvedWorkItem, WorkItemCreationTarget, WorkItemError, WorkItemOrigin, WorkItemResolver,
};
use crate::data::issue::Issue;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::AgentInstance;
use crate::engine::auth::AgentCredentials;
use crate::engine::container::options::ContainerOption;
use crate::engine::issue::IssueSource;

#[derive(Debug, Clone)]
pub struct SpecsAmendFlags {
    pub work_item: String,
    pub non_interactive: bool,
    pub allow_docker: bool,
}

#[derive(Debug, Clone)]
pub enum SpecsSubcommand {
    Amend(SpecsAmendFlags),
}

/// What `create_new_spec` produced: a local file (`created_path`), a remote
/// work item (`created_url`, plus the provider's `number` when it has one),
/// or nothing (an interview for a remote source that was left unedited).
#[derive(Debug, Clone, Serialize)]
pub struct NewSpecOutcome {
    pub interview: bool,
    pub created_path: Option<String>,
    pub created_url: Option<String>,
    pub number: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecsAmendOutcome {
    pub work_item: String,
    pub non_interactive: bool,
    pub allow_docker: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum SpecsOutcome {
    Amend(SpecsAmendOutcome),
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

pub trait SpecsCommandFrontend:
    UserMessageSink + crate::command::commands::agent_setup::AgentLaunchFrontend + Send + Sync
{
    /// Prompt the user for the title of the new spec. Returns the title text.
    /// CLI implementations gate this on `stdin_is_tty()` and fall back to a
    /// generated default when not a TTY.
    fn ask_spec_title(&mut self) -> Result<String, CommandError> {
        Ok("Untitled work item".to_string())
    }

    /// Prompt for a one-line summary.
    fn ask_spec_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
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

    /// Prompt for a summary pre-populated with `content`. The TUI implementation
    /// renders the text box pre-filled; headless frontends return the
    /// content unmodified.
    fn ask_spec_summary_prefilled(&mut self, content: &str) -> Result<String, CommandError> {
        Ok(content.to_string())
    }
}

pub struct SpecsCommand {
    sub: SpecsSubcommand,
    engines: Engines,
    session: Session,
    cancel: CancellationToken,
}

impl SpecsCommand {
    pub fn new(sub: SpecsSubcommand, engines: Engines, session: Session) -> Self {
        Self {
            sub,
            engines,
            session,
            cancel: CancellationToken::new(),
        }
    }

    /// Let an external cancel stop the agent. The draft of a remote work item
    /// is still uploaded once after the agent exits.
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). `specs`
    /// has one leaf today; the subcommand is still selected from the caller's
    /// canonical path so a second one needs no new entry point.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let sub = match ctx.caller.leaf() {
            "amend" => SpecsSubcommand::Amend(SpecsAmendFlags {
                work_item: ctx.args.require("work_item")?,
                non_interactive: ctx.flags.bool("headless"),
                allow_docker: ctx.flags.bool("allow-docker"),
            }),
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(sub, ctx.engines.clone(), ctx.session.clone())
            .with_cancellation(ctx.cancel.clone()))
    }

    pub fn subcommand(&self) -> &SpecsSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for SpecsCommand {
    type Frontend = Box<dyn SpecsCommandFrontend>;
    type Outcome = SpecsOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let outcome = match self.sub {
            SpecsSubcommand::Amend(f) => SpecsOutcome::Amend(
                amend(
                    &self.engines,
                    &self.session,
                    f,
                    &self.cancel,
                    frontend.as_mut(),
                )
                .await?,
            ),
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// `specs amend <work_item>`: resolve the reference with the shared resolver
/// (so `workItems.source` and the reference grammar apply), then edit the
/// local file in place or the remote work item through a synced draft.
async fn amend(
    engines: &Engines,
    session: &Session,
    f: SpecsAmendFlags,
    cancel: &CancellationToken,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<SpecsAmendOutcome, CommandError> {
    const LABEL: &str = "specs amend";
    let resolver = WorkItemResolver::new(session.clone(), Arc::clone(&engines.git_engine));
    let item = match resolver.resolve(Some(&f.work_item), &mut *frontend).await {
        Ok(Some(item)) => item,
        Ok(None) => return Err(report(frontend, LABEL, WorkItemError::MissingInput)),
        Err(e) => return Err(report(frontend, LABEL, e)),
    };
    // Pick the provider before launching anything: an inline item, or a
    // provider that is no longer configured, stops here.
    let remote = match &item.origin {
        WorkItemOrigin::Local { .. } => None,
        WorkItemOrigin::Remote { .. } => {
            let source = resolver
                .source_for(&item)
                .map_err(|e| report(frontend, LABEL, e))?;
            let issue = item
                .issue
                .clone()
                .ok_or(WorkItemError::InlineNotEditable)
                .map_err(|e| report(frontend, LABEL, e))?;
            Some((source, issue))
        }
    };

    let agent = resolve_agent(session, LABEL, frontend)?;
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "{LABEL}: reviewing work item \"{}\" with agent '{}'",
            item.title,
            agent.as_str()
        ),
    });
    let run_opts = AgentRunOptions {
        non_interactive: f.non_interactive,
        allow_docker: f.allow_docker,
        ..Default::default()
    };
    match remote {
        None => {
            let path = local_container_path(session.git_root(), &item);
            let prompt = render_amend_prompt(&path, &item.title);
            run_local_agent(
                engines, session, &agent, run_opts, None, prompt, LABEL, frontend,
            )
            .await?;
        }
        Some((source, issue)) => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "{LABEL}: work item {}",
                    crate::data::issue::display_url(&issue.source_id)
                ),
            });
            let seed = issue.body.clone();
            let title = item.title.clone();
            run_remote_edit(
                engines,
                session,
                RemoteEdit {
                    agent: &agent,
                    run_opts,
                    credentials: None,
                    source,
                    mode: DraftSyncMode::Amend { issue },
                    seed,
                    prompt: &|path| render_amend_prompt(path, &title),
                    label: LABEL,
                },
                cancel,
                frontend,
            )
            .await?;
        }
    }
    Ok(SpecsAmendOutcome {
        work_item: f.work_item,
        non_interactive: f.non_interactive,
        allow_docker: f.allow_docker,
    })
}

/// `new spec` body. Resolves the creation target and the template, runs the
/// Q&A through the supplied frontend, then either writes a numbered local
/// file or creates a remote work item, and (when `interview` is set, or the
/// spec is seeded from `--work-item`) hands it to an agent for completion.
/// Called by `NewCommand::Spec`.
pub(crate) async fn create_new_spec(
    engines: &Engines,
    session: Session,
    interview: bool,
    non_interactive: bool,
    work_item: Option<String>,
    cancel: &CancellationToken,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<NewSpecOutcome, CommandError> {
    const LABEL: &str = "new spec";
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
    // target would be duplicated, so it is refused in favour of `specs amend`.
    let resolver = WorkItemResolver::new(session.clone(), Arc::clone(&engines.git_engine));
    let target = resolver
        .creation_target()
        .map_err(|e| report(frontend, LABEL, e))?;
    let seed: Option<ResolvedWorkItem> =
        match resolver.resolve(work_item.as_deref(), &mut *frontend).await {
            Ok(seed) => seed,
            Err(e) => return Err(report(frontend, LABEL, e)),
        };
    if let Some(seed) = &seed {
        resolver
            .ensure_new_in_target(seed, &target)
            .map_err(|e| report(frontend, LABEL, e))?;
    }

    // When seeded, kind/title/summary come from the seed. The default kind
    // is Task (the safe default, matching the unseeded path); the interview
    // agent can revise it from the content. A remote seed contributes its raw
    // body: the title is passed separately, so no `# <title>` is added.
    let seed_summary = seed.as_ref().map(|s| match &s.issue {
        Some(issue) => issue.body.clone(),
        None => s.content.clone(),
    });
    let (kind, title, summary, slug) = match &seed {
        Some(seed) => (
            WorkItemKind::Task,
            seed.title.clone(),
            seed_summary.clone().unwrap_or_default(),
            seed.slug.clone().unwrap_or_else(|| slugify(&seed.title)),
        ),
        None => {
            // A dismissed question (Ctrl+C, closed dialog) aborts the command;
            // any other failure keeps the historical default.
            let kind = keep_aborted(
                frontend.ask_spec_kind(&crate::command::prompts::work_item_kind()),
                WorkItemKind::Task,
            )?;
            let title = keep_aborted(frontend.ask_spec_title(), "Untitled".into())?;
            let summary = keep_aborted(frontend.ask_spec_summary(), String::new())?;
            let slug = slugify(&title);
            (kind, title, summary, slug)
        }
    };

    // --work-item only: implicit interview with the seed's content.
    // --work-item + --interview: the user may edit the prefilled content.
    // --interview only: normal interview.
    let should_interview = interview || seed.is_some();
    let interview_summary = match &seed_summary {
        Some(content) if interview => keep_aborted(
            frontend.ask_spec_summary_prefilled(content),
            content.clone(),
        )?,
        Some(content) => content.clone(),
        None => summary.clone(),
    };

    match target {
        WorkItemCreationTarget::Local { directory } => {
            let number = crate::data::fs::work_item_files::next_work_item_number(&directory)
                .map_err(|e| report_data(frontend, LABEL, "failed to number the work item", e))?;
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("{LABEL}: creating work item {number:04}"),
            });
            let body = apply_work_item_template(
                &template,
                kind,
                &title,
                &summary,
                &format!("{number:04}"),
            );
            let dest = crate::data::fs::work_item_files::write_new_local_work_item(
                &git_root, &directory, number, &slug, &body,
            )
            .map_err(|e| report_data(frontend, LABEL, "failed to write work item", e))?;

            if should_interview {
                let agent = resolve_agent(&session, LABEL, frontend)?;
                let credentials = resolve_credentials(engines, &session, &agent, LABEL, frontend)?;
                let path = Path::new("/workspace")
                    .join(crate::command::work_item::resolver::repo_relative_display(
                        &git_root, &dest,
                    ))
                    .display()
                    .to_string();
                let prompt =
                    render_interview_prompt(&path, kind.as_str(), &title, &interview_summary);
                let run_opts = AgentRunOptions {
                    non_interactive,
                    env_passthrough: None,
                    ..Default::default()
                };
                run_local_agent(
                    engines,
                    &session,
                    &agent,
                    run_opts,
                    Some(credentials),
                    prompt,
                    LABEL,
                    frontend,
                )
                .await?;
            }
            Ok(NewSpecOutcome {
                interview: should_interview,
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
            // The provider assigns the number when the issue is created, so
            // the template's `{{number}}` renders empty and no local number
            // or directory is touched.
            let body = apply_work_item_template(&template, kind, &title, &summary, "");
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("{LABEL}: creating a {provider} work item in {scope}"),
            });
            if should_interview {
                // Say so before launch: the agent's edits leave the machine
                // while it runs, not only when it exits.
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!(
                        "{LABEL}: draft edits are published to {scope} while the agent runs"
                    ),
                });
            }
            let created = if should_interview {
                let agent = resolve_agent(&session, LABEL, frontend)?;
                let credentials = resolve_credentials(engines, &session, &agent, LABEL, frontend)?;
                let run_opts = AgentRunOptions {
                    non_interactive,
                    env_passthrough: None,
                    ..Default::default()
                };
                let kind = kind.as_str();
                let created = run_remote_edit(
                    engines,
                    &session,
                    RemoteEdit {
                        agent: &agent,
                        run_opts,
                        credentials: Some(credentials),
                        source,
                        mode: DraftSyncMode::Create {
                            title: title.clone(),
                        },
                        seed: body,
                        prompt: &|path| {
                            render_interview_prompt(path, kind, &title, &interview_summary)
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
                        text: format!(
                            "{LABEL}: the draft was not changed, so no work item was created"
                        ),
                    });
                }
                created
            } else {
                if cancel.is_cancelled() {
                    return Err(CommandError::Aborted);
                }
                Some(create_remote_now(engines, &session, source, &title, &body, frontend).await?)
            };
            Ok(NewSpecOutcome {
                interview: should_interview,
                created_path: None,
                number: created.as_ref().and_then(|i| i.number),
                created_url: created.map(|i| i.source_id),
            })
        }
    }
}

/// The answer to a `new spec` question, or `default` when the frontend could
/// not ask it. A dismissed question is `Aborted` and stops the command, so a
/// cancelled dialog can never publish an `Untitled` remote work item.
fn keep_aborted<T>(answer: Result<T, CommandError>, default: T) -> Result<T, CommandError> {
    match answer {
        Ok(value) => Ok(value),
        Err(CommandError::Aborted) => Err(CommandError::Aborted),
        Err(_) => Ok(default),
    }
}

/// Create a remote work item right away (`new spec` without an interview).
/// The body is first written to a recovery draft in the repo context
/// directory; it is deleted once the provider confirms the creation, and kept
/// (with its path printed) when the creation fails.
async fn create_remote_now(
    engines: &Engines,
    session: &Session,
    source: Arc<dyn IssueSource>,
    title: &str,
    body: &str,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<Issue, CommandError> {
    const LABEL: &str = "new spec";
    let git_root = session.git_root().to_path_buf();
    let origin_url = engines.git_engine.remote_effective_url(&git_root, "origin");
    let resolver = crate::data::fs::context_dirs::ContextDirResolver::from_env(session.env())
        .map_err(|e| report_data(frontend, LABEL, "context directory is unavailable", e))?;
    let paths =
        resolver.work_item_draft_paths(origin_url.as_deref(), &git_root, uuid::Uuid::new_v4());
    let draft = crate::data::fs::work_item_draft::WorkItemDraft::create(paths, body)
        .map_err(|e| report_data(frontend, LABEL, "failed to write the recovery draft", e))?;
    let (title, body) = (title.to_string(), body.to_string());
    let result =
        tokio::task::spawn_blocking(move || source.create_issue(&title, &body, &git_root)).await;
    let host_path = draft.paths().host_path.clone();
    let error = match result {
        Ok(Ok(issue)) => {
            announce_created(&issue, LABEL, frontend);
            if let Err(e) = draft.remove() {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "{LABEL}: could not remove the draft at {}: {e}",
                        host_path.display()
                    ),
                });
            }
            return Ok(issue);
        }
        Ok(Err(e)) => CommandError::WorkItem(WorkItemError::Provider(e)),
        Err(e) => CommandError::Other(format!("work item creation task failed: {e}")),
    };
    frontend.write_message(UserMessage {
        level: MessageLevel::Error,
        text: format!(
            "{LABEL}: failed to create the work item: {error}; draft kept at {}",
            host_path.display()
        ),
    });
    Err(error)
}

fn announce_created(issue: &Issue, label: &str, frontend: &mut dyn SpecsCommandFrontend) {
    let number = issue.number.map(|n| format!(" (#{n})")).unwrap_or_default();
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("{label}: created work item {}{number}", issue.source_id),
    });
}

/// Everything `run_remote_edit` needs besides the engines and frontend.
struct RemoteEdit<'a> {
    agent: &'a AgentName,
    /// The run's flags; the prompt and context overlays are filled in here.
    run_opts: AgentRunOptions,
    credentials: Option<AgentCredentials>,
    source: Arc<dyn IssueSource>,
    mode: DraftSyncMode,
    /// The draft's initial content: the raw issue body, never
    /// `format_as_markdown` (which would prepend another `# <title>`).
    seed: String,
    /// Renders the agent prompt from the draft's container path.
    prompt: &'a (dyn Fn(&str) -> String + Sync),
    label: &'static str,
}

/// Edit a remote work item through a draft: write the draft into the
/// `context(repo)` overlay (added read-write, refused if config narrows it to
/// read-only), print its host path, launch the agent, sync the draft while it
/// runs, and upload it once more after the agent exits however it exits.
///
/// Returns the created or updated issue (`None` when an interview made no
/// edit). A failed or paused sync keeps the draft, prints its path and fails
/// the command; on success the draft is deleted.
async fn run_remote_edit(
    engines: &Engines,
    session: &Session,
    edit: RemoteEdit<'_>,
    cancel: &CancellationToken,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<Option<Issue>, CommandError> {
    let RemoteEdit {
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
        .collected_overlays(Vec::new(), None, None)?;
    let prepared = prepare_draft_edit(
        session,
        origin_url.as_deref(),
        &mut collected,
        &seed,
        &mut *frontend,
    )
    .map_err(|e| report(frontend, label, e))?;
    let host_path = prepared.draft.paths().host_path.clone();
    let container_path = prepared.draft.paths().container_path.display().to_string();

    // Until the agent runs nothing can have edited the draft: it holds only
    // the seed (a copy of the issue body), so a failure deletes it rather
    // than leaving private issue text in the context directory.
    let discard_draft = |draft: crate::data::fs::work_item_draft::WorkItemDraft,
                         frontend: &mut dyn SpecsCommandFrontend,
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
                run_opts.initial_prompt = Some(prompt(&container_path));
                build_instance(
                    engines,
                    session,
                    agent,
                    &run_opts,
                    credentials,
                    label,
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
    frontend.replay_queued();

    // The helper already printed a sync error with the draft's path.
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

/// Launch the agent on a local work item file and wait for it. Today's
/// behaviour: the agent's exit status does not fail the command.
#[allow(clippy::too_many_arguments)]
async fn run_local_agent(
    engines: &Engines,
    session: &Session,
    agent: &AgentName,
    mut run_opts: AgentRunOptions,
    credentials: Option<AgentCredentials>,
    prompt: String,
    label: &'static str,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<(), CommandError> {
    // Sandbox-class runtimes: agent spawn lands in WI 0090.
    engines
        .require_container_runtime()
        .map_err(CommandError::from)?;
    run_opts.initial_prompt = Some(prompt);
    let instance = build_instance(
        engines,
        session,
        agent,
        &run_opts,
        credentials,
        label,
        frontend,
    )?;
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
            return Err(CommandError::from(e));
        }
    };
    let _ = execution.wait().await;
    frontend.set_pty_active(false);
    frontend.replay_queued();
    Ok(())
}

/// Build the agent's container instance. `credentials` are resolved by the
/// caller (`new spec`); `specs amend` launches without them, as before.
fn build_instance(
    engines: &Engines,
    session: &Session,
    agent: &AgentName,
    run_opts: &AgentRunOptions,
    credentials: Option<AgentCredentials>,
    label: &str,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<Box<dyn AgentInstance>, CommandError> {
    let built = match &credentials {
        Some(credentials) => engines.agent_engine.build_options_with_credentials(
            session,
            agent,
            run_opts,
            credentials,
        ),
        None => engines.agent_engine.build_options(session, agent, run_opts),
    };
    let mut options = match built {
        Ok(o) => o,
        Err(e) => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to build agent options: {e}"),
            });
            return Err(CommandError::from(e));
        }
    };
    if let Some(credentials) = credentials {
        if !credentials.env_vars.is_empty()
            && matches!(
                credentials.delivery,
                crate::engine::auth::CredentialDelivery::Env
            )
        {
            options.push(ContainerOption::AgentCredentials {
                env_vars: credentials.env_vars,
            });
        }
    }
    crate::engine::agent_runtime::ResolvedAgentOptions::container(options)
        .and_then(|o| engines.runtime.build(o))
        .map_err(|e| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to build container instance: {e}"),
            });
            CommandError::from(e)
        })
}

fn resolve_agent(
    session: &Session,
    label: &str,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<AgentName, CommandError> {
    LaunchPolicy::for_session(session)
        .resolve_agent(&None)
        .inspect_err(|e| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to resolve agent: {e}"),
            });
        })
}

fn resolve_credentials(
    engines: &Engines,
    session: &Session,
    agent: &AgentName,
    label: &str,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<AgentCredentials, CommandError> {
    engines
        .auth_engine
        .resolve_agent_auth(session, agent)
        .map_err(|e| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to resolve agent auth: {e}"),
            });
            CommandError::from(e)
        })
}

/// The container path of a local work item file (the checkout is mounted at
/// `/workspace`).
fn local_container_path(git_root: &Path, item: &ResolvedWorkItem) -> String {
    match &item.origin {
        WorkItemOrigin::Local { path } => Path::new("/workspace")
            .join(crate::command::work_item::resolver::repo_relative_display(
                git_root, path,
            ))
            .display()
            .to_string(),
        WorkItemOrigin::Remote { .. } => String::new(),
    }
}

fn report(
    frontend: &mut dyn SpecsCommandFrontend,
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
    frontend: &mut dyn SpecsCommandFrontend,
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
fn slugify(input: &str) -> String {
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
