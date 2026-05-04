//! `SpecsCommand` — `specs new` and `specs amend`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::chat::{open_session_for_cwd, resolve_agent};
use crate::command::commands::implement_prompts::{render_amend_prompt, render_interview_prompt};
use crate::command::commands::mount_scope::MountScopeFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::container::options::ContainerOption;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct SpecsNewFlags {
    pub interview: bool,
    pub non_interactive: bool,
}

#[derive(Debug, Clone)]
pub struct SpecsAmendFlags {
    pub work_item: String,
    pub non_interactive: bool,
    pub allow_docker: bool,
}

#[derive(Debug, Clone)]
pub enum SpecsSubcommand {
    New(SpecsNewFlags),
    Amend(SpecsAmendFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecsNewOutcome {
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
    New(SpecsNewOutcome),
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
    fn write_stdout(&mut self, _bytes: &[u8]) -> Result<(), crate::engine::error::EngineError> {
        Ok(())
    }
    fn write_stderr(&mut self, _bytes: &[u8]) -> Result<(), crate::engine::error::EngineError> {
        Ok(())
    }
    async fn read_stdin(
        &mut self,
        _buf: &mut [u8],
    ) -> Result<usize, crate::engine::error::EngineError> {
        Ok(0)
    }
    fn report_status(
        &mut self,
        _status: crate::engine::container::frontend::ContainerStatus,
    ) {
    }
    fn report_progress(
        &mut self,
        _progress: crate::engine::container::frontend::ContainerProgress,
    ) {
    }
    fn resize_pty(&mut self, _cols: u16, _rows: u16) {}
}

pub struct SpecsCommand {
    sub: SpecsSubcommand,
    engines: Engines,
}

impl SpecsCommand {
    pub fn new(sub: SpecsSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
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
            SpecsSubcommand::New(f) => {
                let new_outcome = create_new_spec(
                    &self.engines,
                    f.interview,
                    f.non_interactive,
                    frontend.as_mut(),
                )
                .await?;
                SpecsOutcome::New(new_outcome)
            }
            SpecsSubcommand::Amend(f) => {
                let session = open_session_for_cwd(&self.engines)?;
                let git_root = session.git_root().to_path_buf();
                let work_items_dir = session
                    .repo_config()
                    .work_items_dir_or_default(&git_root);
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
                    return Err(CommandError::WorkItemNotFound { number: n });
                }

                // Run the amend agent to review the file against the
                // implementation. Honors --non-interactive and --allow-docker.
                let agent = resolve_agent(&None, &session)?;
                let prompt = render_amend_prompt(n);
                let run_opts = AgentRunOptions {
                    initial_prompt: Some(prompt),
                    non_interactive: f.non_interactive,
                    allow_docker: f.allow_docker,
                    ..Default::default()
                };
                let options = self
                    .engines
                    .agent_engine
                    .build_options(&session, &agent, &run_opts)?;
                let instance = self.engines.runtime.build(options)?;
                frontend.set_pty_active(true);
                let cf = frontend.container_frontend();
                let mut execution = match instance.run_with_frontend(cf) {
                    Ok(e) => e,
                    Err(e) => {
                        frontend.set_pty_active(false);
                        frontend.replay_queued();
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

/// Shared `specs new` / `new spec` body. Resolves the work-items dir + the
/// template, runs the Q&A through the supplied frontend, writes the
/// substituted file, and (when `interview` is set) hands the bare file to
/// an agent for completion. Reused by both `SpecsCommand::SpecsNew` and
/// `NewCommand::Spec` since dispatch canonicalizes `specs new` → `new spec`.
pub(crate) async fn create_new_spec(
    engines: &crate::command::dispatch::Engines,
    interview: bool,
    non_interactive: bool,
    frontend: &mut dyn SpecsCommandFrontend,
) -> Result<SpecsNewOutcome, CommandError> {
    let session = open_session_for_cwd(engines)?;
    let git_root = session.git_root().to_path_buf();
    let work_items_dir = session
        .repo_config()
        .work_items_dir_or_default(&git_root);
    let template_path = session
        .repo_config()
        .work_items_template_or_default(&git_root);

    if !template_path.exists() {
        return Err(CommandError::SpecTemplateMissing {
            path: template_path.clone(),
        });
    }
    let template = std::fs::read_to_string(&template_path).map_err(|e| {
        CommandError::Other(format!(
            "reading spec template {}: {e}",
            template_path.display()
        ))
    })?;

    let next_n = next_work_item_number(&work_items_dir);
    let kind = frontend.ask_spec_kind().unwrap_or(WorkItemKind::Task);
    let title = frontend.ask_spec_title().unwrap_or_else(|_| "Untitled".into());
    let summary = frontend.ask_spec_summary().unwrap_or_default();
    let slug = slugify(&title);
    let filename = format!("{:04}-{slug}.md", next_n);
    let dest = work_items_dir.join(&filename);

    std::fs::create_dir_all(&work_items_dir).map_err(|e| {
        CommandError::Other(format!(
            "creating work-items dir {}: {e}",
            work_items_dir.display()
        ))
    })?;

    let number_str = format!("{next_n:04}");
    let body = apply_work_item_template(&template, kind, &title, &summary, &number_str);
    std::fs::write(&dest, body)
        .map_err(|e| CommandError::Other(format!("writing work item {}: {e}", dest.display())))?;

    if interview {
        let agent = resolve_agent(&None, &session)?;
        let credentials = engines
            .auth_engine
            .resolve_agent_auth(&session, &agent)
            .map_err(CommandError::from)?;
        let prompt = render_interview_prompt(next_n, kind.as_str(), &title, &summary);
        let run_opts = AgentRunOptions {
            initial_prompt: Some(prompt),
            non_interactive,
            env_passthrough: Some(session.effective_config().env_passthrough()),
            ..Default::default()
        };
        let mut options = engines
            .agent_engine
            .build_options(&session, &agent, &run_opts)?;
        if !credentials.env_vars.is_empty() {
            options.push(ContainerOption::AgentCredentials {
                env_vars: credentials.env_vars,
            });
        }
        let instance = engines.runtime.build(options)?;
        frontend.set_pty_active(true);
        let cf = frontend.container_frontend();
        let mut execution = match instance.run_with_frontend(cf) {
            Ok(e) => e,
            Err(e) => {
                frontend.set_pty_active(false);
                frontend.replay_queued();
                return Err(CommandError::from(e));
            }
        };
        let _ = execution.wait().await;
        frontend.set_pty_active(false);
        frontend.replay_queued();
    }

    Ok(SpecsNewOutcome {
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

    // ── SpecsCommand::New tests ──────────────────────────────────────────────

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
        use std::sync::Arc;
        use crate::engine::overlay::OverlayEngine;
        use crate::engine::container::ContainerRuntime;
        use crate::data::fs::auth_paths::AuthPathResolver;
        use crate::data::fs::headless_paths::HeadlessPaths;
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
            HeadlessPaths::at_root(root),
        ));
        crate::command::dispatch::Engines {
            runtime,
            git_engine: Arc::new(crate::engine::git::GitEngine::new()),
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store: Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(root)),
        }
    }

    /// Run `f` with the process CWD set to `dir`, restoring it afterward.
    /// Holds `CWD_LOCK` for the full duration to prevent races.
    #[allow(clippy::await_holding_lock)]
    async fn with_cwd<F, Fut, T>(dir: &std::path::Path, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _lock = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        std::env::set_current_dir(dir).unwrap();
        let result = f().await;
        let _ = std::env::set_current_dir(&prev);
        result
    }

    #[tokio::test]
    async fn specs_new_requires_template_to_exist_returns_error_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines_with_root(tmp.path());
        let cmd = super::SpecsCommand::new(
            super::SpecsSubcommand::New(super::SpecsNewFlags { interview: false, non_interactive: false }),
            engines,
        );
        let result = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeSpecsFrontend)).await
        }).await;
        assert!(result.is_err(), "must error when template is missing");
    }

    #[tokio::test]
    async fn specs_new_writes_file_when_template_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let work_items = tmp.path().join("aspec").join("work-items");
        std::fs::create_dir_all(&work_items).unwrap();
        let template = work_items.join("0000-template.md");
        // Template matches the real 0000-template.md format.
        std::fs::write(
            &template,
            "# Work Item: [Feature | Bug | Task]\n\nTitle: title\n\n- summary\n",
        ).unwrap();

        let engines = make_engines_with_root(tmp.path());
        let cmd = super::SpecsCommand::new(
            super::SpecsSubcommand::New(super::SpecsNewFlags { interview: false, non_interactive: false }),
            engines,
        );
        let outcome = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeSpecsFrontend)).await.unwrap()
        }).await;
        if let super::SpecsOutcome::New(n) = outcome {
            let path = n.created_path.expect("created_path must be Some");
            assert!(
                std::path::Path::new(&path).exists(),
                "created file must exist on disk: {path}"
            );
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.contains("My Test Spec"), "title must be substituted: {content}");
            assert!(content.contains("# Work Item: Task"), "kind must be substituted: {content}");
            assert!(content.contains("A one-line summary."), "summary must be substituted: {content}");
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn specs_new_interview_writes_file_then_invokes_agent() {
        // With --interview, after writing the bare file the command attempts
        // to run the interview agent. In a test environment without Docker
        // the runtime spawn fails — we tolerate that as long as the file
        // landed first (proving the substitution / write path completed
        // before the agent step).
        let tmp = tempfile::tempdir().unwrap();
        let work_items = tmp.path().join("aspec").join("work-items");
        std::fs::create_dir_all(&work_items).unwrap();
        let template = work_items.join("0000-template.md");
        std::fs::write(&template, "# Work Item: [Feature | Bug | Task]\n\nTitle: title\n").unwrap();

        let engines = make_engines_with_root(tmp.path());
        let cmd = super::SpecsCommand::new(
            super::SpecsSubcommand::New(super::SpecsNewFlags { interview: true, non_interactive: false }),
            engines,
        );
        let _ = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeSpecsFrontend)).await
        }).await;

        // File must have been written before the agent run was attempted.
        let entries: Vec<_> = std::fs::read_dir(&work_items)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("0001-"))
            .collect();
        assert!(
            !entries.is_empty(),
            "interview must write the bare file before running the agent"
        );
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
        let cmd = super::SpecsCommand::new(
            super::SpecsSubcommand::Amend(super::SpecsAmendFlags {
                work_item: "0042".to_string(),
                non_interactive: true,
                allow_docker: false,
            }),
            engines,
        );
        let result = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeSpecsFrontend)).await
        }).await;
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
        let cmd = super::SpecsCommand::new(
            super::SpecsSubcommand::Amend(super::SpecsAmendFlags {
                work_item: "9999".to_string(),
                non_interactive: false,
                allow_docker: false,
            }),
            engines,
        );
        let result = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeSpecsFrontend)).await
        }).await;
        assert!(result.is_err(), "must return error when work item 9999 not found");
    }
}
