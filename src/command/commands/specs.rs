//! `SpecsCommand` — `specs amend`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::prompt_templates::{render_amend_prompt, render_interview_prompt};
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::ContainerOption;

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

#[derive(Debug, Clone, Serialize)]
pub struct NewSpecOutcome {
    pub interview: bool,
    pub created_path: Option<String>,
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
    session: crate::data::session::Session,
}

impl SpecsCommand {
    pub fn new(
        sub: SpecsSubcommand,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            sub,
            engines,
            session,
        }
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
        Ok(Self::new(sub, ctx.engines.clone(), ctx.session.clone()))
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
            SpecsSubcommand::Amend(f) => {
                let session = self.session;
                let git_root = session.git_root().to_path_buf();
                let work_items_dir = session.effective_config().work_items_dir(&git_root);
                // Look up the file for the requested work-item number.
                let n: u32 = f.work_item.trim_start_matches('0').parse().unwrap_or(0);
                let prefix = format!("{:04}-", n);
                let mut found: Option<std::path::PathBuf> = None;
                if let Ok(entries) = std::fs::read_dir(&work_items_dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let s = name.to_string_lossy();
                        if s.starts_with(&prefix) && s.ends_with(".md") {
                            found = Some(entry.path());
                            break;
                        }
                    }
                }
                if found.is_none() {
                    let err = CommandError::WorkItemNotFound { number: n };
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("specs amend: work item {:04} not found", n),
                    });
                    return Err(err);
                }

                // Run the amend agent to review the file against the
                // implementation. Honors --headless and --allow-docker.
                let agent = match LaunchPolicy::for_session(&session).resolve_agent(&None) {
                    Ok(a) => a,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("specs amend: failed to resolve agent: {e}"),
                        });
                        return Err(e);
                    }
                };
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!(
                        "specs amend: reviewing work item {:04} with agent '{}'",
                        n,
                        agent.as_str()
                    ),
                });
                let prompt = render_amend_prompt(n);
                let run_opts = AgentRunOptions {
                    initial_prompt: Some(prompt),
                    non_interactive: f.non_interactive,
                    allow_docker: f.allow_docker,
                    ..Default::default()
                };
                // Sandbox-class runtimes: agent spawn lands in WI 0090.
                self.engines
                    .require_container_runtime()
                    .map_err(CommandError::from)?;
                let options = match self
                    .engines
                    .agent_engine
                    .build_options(&session, &agent, &run_opts)
                {
                    Ok(o) => o,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("specs amend: failed to build agent options: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                let instance =
                    match crate::engine::agent_runtime::ResolvedAgentOptions::container(options)
                        .and_then(|o| self.engines.runtime.build(o))
                    {
                        Ok(i) => i,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!(
                                    "specs amend: failed to build container instance: {e}"
                                ),
                            });
                            return Err(CommandError::from(e));
                        }
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
                            text: format!("specs amend: failed to run container: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                let _ = execution.wait().await;
                frontend.set_pty_active(false);
                frontend.replay_queued();

                SpecsOutcome::Amend(SpecsAmendOutcome {
                    work_item: f.work_item,
                    non_interactive: f.non_interactive,
                    allow_docker: f.allow_docker,
                })
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// `new spec` body. Resolves the work-items dir + the template, runs the
/// Q&A through the supplied frontend, writes the substituted file, and
/// (when `interview` is set) hands the bare file to an agent for completion.
/// Called by `NewCommand::Spec`.
pub(crate) async fn create_new_spec(
    engines: &crate::command::dispatch::Engines,
    session: crate::data::session::Session,
    interview: bool,
    non_interactive: bool,
    issue_flags: crate::engine::issue::IssueSourceFlags,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<NewSpecOutcome, CommandError> {
    let git_root = session.git_root().to_path_buf();
    let config = session.effective_config();
    let work_items_dir = config.work_items_dir(&git_root);
    let template_path = config.work_items_template(&git_root);

    if !template_path.exists() {
        let err = CommandError::SpecTemplateMissing {
            path: template_path.clone(),
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Error,
            text: format!(
                "new spec: spec template missing at {}",
                template_path.display()
            ),
        });
        return Err(err);
    }
    let template = match std::fs::read_to_string(&template_path) {
        Ok(t) => t,
        Err(e) => {
            let err = CommandError::Other(format!(
                "reading spec template {}: {e}",
                template_path.display()
            ));
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "new spec: failed to read spec template {}: {e}",
                    template_path.display()
                ),
            });
            return Err(err);
        }
    };

    // Fetch issue if --issue was provided. Extract everything we need while the
    // router is alive so we don't hold a borrow across its drop.
    struct FetchedIssue {
        title: String,
        markdown: String,
        slug: String,
    }
    let fetched_issue: Option<FetchedIssue> = if let Some(ref issue_ref) = issue_flags.issue {
        let router = crate::engine::issue::router::IssueSourceRouter::new(
            std::sync::Arc::clone(&engines.git_engine),
            session.env(),
        );
        match router.fetch_issue_with_progress(issue_ref, &git_root, frontend) {
            Ok((issue, source)) => {
                let markdown = source.format_as_markdown(&issue);
                let slug = source.title_slug(&issue);
                let title = issue.title.clone();
                Some(FetchedIssue {
                    title,
                    markdown,
                    slug,
                })
            }
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("new spec: failed to fetch issue: {e}"),
                });
                return Err(CommandError::Other(e.to_string()));
            }
        }
    } else {
        None
    };

    let next_n = next_work_item_number(&work_items_dir);
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("new spec: creating work item {:04}", next_n),
    });

    // When --issue is set, derive kind/title/summary/slug from the issue.
    // Default kind is Task (the safe default, matching the non-issue path);
    // the interview agent can revise it from the issue content.
    let (kind, title, summary, slug) = if let Some(ref fi) = fetched_issue {
        let kind = WorkItemKind::Task;
        let title = fi.title.clone();
        let summary = fi.markdown.clone();
        let slug = fi.slug.clone();
        (kind, title, summary, slug)
    } else {
        let kind = frontend
            .ask_spec_kind(&crate::command::prompts::work_item_kind())
            .unwrap_or(WorkItemKind::Task);
        let title = frontend
            .ask_spec_title()
            .unwrap_or_else(|_| "Untitled".into());
        let summary = frontend.ask_spec_summary().unwrap_or_default();
        let slug = slugify(&title);
        (kind, title, summary, slug)
    };
    let filename = format!("{:04}-{slug}.md", next_n);
    let dest = work_items_dir.join(&filename);

    match std::fs::create_dir_all(&work_items_dir) {
        Ok(()) => {}
        Err(e) => {
            let err = CommandError::Other(format!(
                "creating work-items dir {}: {e}",
                work_items_dir.display()
            ));
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "new spec: failed to create work-items dir {}: {e}",
                    work_items_dir.display()
                ),
            });
            return Err(err);
        }
    }

    let number_str = format!("{next_n:04}");
    let body = apply_work_item_template(&template, kind, &title, &summary, &number_str);
    match std::fs::write(&dest, body) {
        Ok(()) => {}
        Err(e) => {
            let err = CommandError::Other(format!("writing work item {}: {e}", dest.display()));
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "new spec: failed to write work item {}: {e}",
                    dest.display()
                ),
            });
            return Err(err);
        }
    }

    // Determine if we should launch the interview agent.
    // --issue only (no --interview): implicit interview with issue content.
    // --issue + --interview: prefilled interview with editable content.
    // --interview only: normal interview.
    let should_interview = interview || fetched_issue.is_some();
    let interview_summary = if let Some(ref fi) = fetched_issue {
        if interview {
            // --issue + --interview: let user edit the prefilled content
            match frontend.ask_spec_summary_prefilled(&fi.markdown) {
                Ok(edited) => edited,
                Err(_) => fi.markdown.clone(),
            }
        } else {
            // --issue only: use issue content directly
            fi.markdown.clone()
        }
    } else {
        summary.clone()
    };

    if should_interview {
        let agent = match LaunchPolicy::for_session(&session).resolve_agent(&None) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("new spec: failed to resolve agent: {e}"),
                });
                return Err(e);
            }
        };
        let credentials = match engines.auth_engine.resolve_agent_auth(&session, &agent) {
            Ok(c) => c,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("new spec: failed to resolve agent auth: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        let prompt = render_interview_prompt(next_n, kind.as_str(), &title, &interview_summary);
        let run_opts = AgentRunOptions {
            initial_prompt: Some(prompt),
            non_interactive,
            env_passthrough: None,
            ..Default::default()
        };
        // Sandbox-class runtimes: agent spawn lands in WI 0090.
        engines
            .require_container_runtime()
            .map_err(CommandError::from)?;
        let mut options = match engines.agent_engine.build_options_with_credentials(
            &session,
            &agent,
            &run_opts,
            &credentials,
        ) {
            Ok(o) => o,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("new spec: failed to build agent options: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
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
        let instance = match crate::engine::agent_runtime::ResolvedAgentOptions::container(options)
            .and_then(|o| engines.runtime.build(o))
        {
            Ok(i) => i,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("new spec: failed to build container instance: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Launching interview agent…".into(),
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
                    text: format!("new spec: failed to run container: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        let _ = execution.wait().await;
        frontend.set_pty_active(false);
        frontend.replay_queued();
    }

    Ok(NewSpecOutcome {
        interview: should_interview,
        created_path: Some(dest.display().to_string()),
    })
}

/// Compute the next work-item number by scanning `dir` for `NNNN-*.md`.
fn next_work_item_number(dir: &std::path::Path) -> u32 {
    let mut max = 0u32;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.len() >= 5 && s.as_bytes()[4] == b'-' {
                if let Ok(n) = s[..4].parse::<u32>() {
                    if n > max {
                        max = n;
                    }
                }
            }
        }
    }
    max + 1
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
