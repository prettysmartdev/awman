//! `SpecsCommand` — `specs amend`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::mount_scope::MountScopeFrontend;
use crate::command::commands::prompt_templates::{render_amend_prompt, render_interview_prompt};
use crate::command::commands::{resolve_agent, Command};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::container::options::ContainerOption;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

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

/// Work-item kinds — `oldsrc/commands/new.rs::WorkItemKind` ported verbatim.
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
    UserMessageSink + MountScopeFrontend + AgentSetupFrontend + AgentAuthFrontend + Send + Sync
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

    /// Prompt the user for the work-item kind. Default: `Task`, matching the
    /// safe-default-on-pipe behavior expected of every Q&A method.
    fn ask_spec_kind(&mut self) -> Result<WorkItemKind, CommandError> {
        Ok(WorkItemKind::Task)
    }

    /// Hand back a container-side frontend for spawning the interview / amend
    /// agent. Default impl returns a no-op proxy; CLI / TUI override.
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(NoopContainerFrontend)
    }

    /// Like `container_frontend`, but yields a frontend that surrenders its
    /// PTY I/O channels for direct bridging. Interactive container launches
    /// call this so the PTY is wired to the TUI renderer.
    /// Default falls back to `container_frontend`.
    fn container_frontend_for_pty(&mut self) -> Box<dyn ContainerFrontend> {
        self.container_frontend()
    }

    /// PTY lifecycle gating around the agent run. Default: no-op.
    fn set_pty_active(&mut self, _active: bool) {}
}

/// A minimal `ContainerFrontend` used as a default when the per-frontend
/// impl doesn't supply one. Suitable for non-interactive command paths and
/// tests that don't actually run a container.
struct NoopContainerFrontend;

impl crate::engine::message::UserMessageSink for NoopContainerFrontend {
    fn write_message(&mut self, _: crate::engine::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl ContainerFrontend for NoopContainerFrontend {
    fn report_status(&mut self, _status: crate::engine::container::frontend::ContainerStatus) {}
    fn report_progress(
        &mut self,
        _progress: crate::engine::container::frontend::ContainerProgress,
    ) {
    }
    fn take_container_io(&mut self) -> crate::engine::container::frontend::ContainerIo {
        let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::engine::container::frontend::ContainerIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
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
                let work_items_dir = session.repo_config().work_items_dir_or_default(&git_root);
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
                // implementation. Honors --non-interactive and --allow-docker.
                let agent = match resolve_agent(&None, &session) {
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
                let instance = match self.engines.runtime.build(options) {
                    Ok(i) => i,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("specs amend: failed to build container instance: {e}"),
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
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<NewSpecOutcome, CommandError> {
    let git_root = session.git_root().to_path_buf();
    let work_items_dir = session.repo_config().work_items_dir_or_default(&git_root);
    let template_path = session
        .repo_config()
        .work_items_template_or_default(&git_root);

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

    let next_n = next_work_item_number(&work_items_dir);
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("new spec: creating work item {:04}", next_n),
    });
    let kind = frontend.ask_spec_kind().unwrap_or(WorkItemKind::Task);
    let title = frontend
        .ask_spec_title()
        .unwrap_or_else(|_| "Untitled".into());
    let summary = frontend.ask_spec_summary().unwrap_or_default();
    let slug = slugify(&title);
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

    if interview {
        let agent = match resolve_agent(&None, &session) {
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
        let prompt = render_interview_prompt(next_n, kind.as_str(), &title, &summary);
        let run_opts = AgentRunOptions {
            initial_prompt: Some(prompt),
            non_interactive,
            env_passthrough: None,
            ..Default::default()
        };
        let mut options = match engines
            .agent_engine
            .build_options(&session, &agent, &run_opts)
        {
            Ok(o) => o,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("new spec: failed to build agent options: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        if !credentials.env_vars.is_empty() {
            options.push(ContainerOption::AgentCredentials {
                env_vars: credentials.env_vars,
            });
        }
        let instance = match engines.runtime.build(options) {
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
        interview,
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

/// Apply all work-item substitutions to a template string. Ported from
/// `oldsrc/commands/new.rs::apply_template` and extended with number/summary.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic_lowercases_and_hyphenates() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Foo!Bar?"), "foo-bar");
        assert_eq!(slugify("  trim  edges  "), "trim-edges");
        assert_eq!(slugify(""), "untitled");
    }

    #[test]
    fn next_work_item_number_empty_dir_starts_at_one() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(next_work_item_number(tmp.path()), 1);
    }

    #[test]
    fn next_work_item_number_finds_max() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("0001-a.md"), "").unwrap();
        std::fs::write(tmp.path().join("0042-b.md"), "").unwrap();
        std::fs::write(tmp.path().join("0007-c.md"), "").unwrap();
        assert_eq!(next_work_item_number(tmp.path()), 43);
    }

    // ── SpecsCommand::Amend tests ────────────────────────────────────────────

    struct FakeSpecsFrontend;
    impl crate::engine::message::UserMessageSink for FakeSpecsFrontend {
        fn write_message(&mut self, _: crate::engine::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl crate::command::commands::mount_scope::MountScopeFrontend for FakeSpecsFrontend {
        fn ask_mount_scope(
            &mut self,
            _git_root: &std::path::Path,
            _cwd: &std::path::Path,
        ) -> Result<
            crate::command::commands::mount_scope::MountScopeDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::mount_scope::MountScopeDecision::MountGitRoot)
        }
    }
    impl crate::command::commands::agent_setup::AgentSetupFrontend for FakeSpecsFrontend {
        fn ask_agent_setup(
            &mut self,
            _requested: &crate::data::session::AgentName,
            _default: &crate::data::session::AgentName,
            _default_available: bool,
            _image_only: bool,
        ) -> Result<
            crate::command::commands::agent_setup::AgentSetupDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::agent_setup::AgentSetupDecision::Setup)
        }
        fn record_fallback(
            &mut self,
            _requested: &crate::data::session::AgentName,
            _fallback: &crate::data::session::AgentName,
        ) {
        }
    }
    impl crate::command::commands::agent_auth::AgentAuthFrontend for FakeSpecsFrontend {
        fn ask_agent_auth_consent(
            &mut self,
            _agent: &crate::data::session::AgentName,
            _env_var_names: &[&str],
        ) -> Result<
            crate::command::commands::agent_auth::AgentAuthDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::agent_auth::AgentAuthDecision::Decline)
        }
    }
    impl super::SpecsCommandFrontend for FakeSpecsFrontend {
        fn ask_spec_title(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok("My Test Spec".to_string())
        }
        fn ask_spec_summary(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok("A one-line summary.".to_string())
        }
    }

    fn make_engines_with_root(root: &std::path::Path) -> crate::command::dispatch::Engines {
        use crate::data::fs::api_paths::ApiPaths;
        use crate::data::fs::auth_paths::AuthPathResolver;
        use crate::engine::container::ContainerRuntime;
        use crate::engine::overlay::OverlayEngine;
        use std::sync::Arc;
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(root),
        ));
        let runtime = Arc::new(ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            AuthPathResolver::at_home(root),
            ApiPaths::at_root(root),
        ));
        crate::command::dispatch::Engines {
            runtime,
            git_engine: Arc::new(crate::engine::git::GitEngine::new()),
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store: Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                root,
            )),
        }
    }

    fn make_session(root: &std::path::Path) -> crate::data::session::Session {
        let resolver = crate::data::session::StaticGitRootResolver::new(root);
        crate::data::session::Session::open(
            root.to_path_buf(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn specs_amend_locates_file_then_invokes_agent() {
        // After locating the file, amend invokes the agent. In a test env
        // without Docker that spawn fails, but we can still check that the
        // file lookup succeeded by asserting the error is NOT
        // `WorkItemNotFound`.
        let tmp = tempfile::tempdir().unwrap();
        let work_items = tmp.path().join("aspec").join("work-items");
        std::fs::create_dir_all(&work_items).unwrap();
        std::fs::write(work_items.join("0042-my-feature.md"), "# My Feature").unwrap();

        let engines = make_engines_with_root(tmp.path());
        let session = make_session(tmp.path());
        let cmd = super::SpecsCommand::new(
            super::SpecsSubcommand::Amend(super::SpecsAmendFlags {
                work_item: "0042".to_string(),
                non_interactive: true,
                allow_docker: false,
            }),
            engines,
            session,
        );
        let result = cmd.run_with_frontend(Box::new(FakeSpecsFrontend)).await;
        if let Err(crate::command::error::CommandError::WorkItemNotFound { .. }) = &result {
            panic!("file lookup must succeed for an existing work item: {result:?}");
        }
    }

    #[tokio::test]
    async fn specs_amend_returns_error_when_work_item_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let work_items = tmp.path().join("aspec").join("work-items");
        std::fs::create_dir_all(&work_items).unwrap();

        let engines = make_engines_with_root(tmp.path());
        let session = make_session(tmp.path());
        let cmd = super::SpecsCommand::new(
            super::SpecsSubcommand::Amend(super::SpecsAmendFlags {
                work_item: "9999".to_string(),
                non_interactive: false,
                allow_docker: false,
            }),
            engines,
            session,
        );
        let result = cmd.run_with_frontend(Box::new(FakeSpecsFrontend)).await;
        assert!(
            result.is_err(),
            "must return error when work item 9999 not found"
        );
    }
}
