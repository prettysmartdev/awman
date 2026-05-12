//! `HeadlessDispatchFrontend` — the single Layer 3 struct that implements
//! every per-command frontend trait for headless HTTP command dispatch.
//!
//! When a `POST /v1/commands` request arrives, the route handler constructs
//! a `HeadlessDispatchFrontend` pre-loaded with the parsed args/flags from
//! the HTTP request body, then hands it to `Dispatch::run_command`. All
//! output (UserMessages, container stdout/stderr) is written to the
//! command's `output.log` file on disk. SSE clients tailing the log see
//! new lines in real time.
//!
//! All interactive Q&A methods return safe non-interactive defaults (the
//! same defaults the CLI uses when stdin is not a TTY).

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{
    AgentSetupDecision, AgentSetupFrontend, HasContainerFrontend,
};
use crate::command::commands::auth::AuthCommandFrontend;
use crate::command::commands::chat::ChatCommandFrontend;
use crate::command::commands::config::{ConfigCommandFrontend, ConfigEditRequest, ConfigFieldRow};
use crate::command::commands::download::DownloadCommandFrontend;
use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;
use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::command::commands::headless::HeadlessCommandFrontend;
use crate::command::commands::headless::HeadlessServeConfig;
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::new::NewCommandFrontend;
use crate::command::commands::remote::RemoteCommandFrontend;
use crate::command::commands::specs::SpecsCommandFrontend;
use crate::command::commands::status::StatusCommandFrontend;
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PreWorktreeDecision,
    WorktreeLifecycleFrontend,
};
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::data::config::repo::WorkItemsConfig;
use crate::data::session::AgentName;
use crate::data::workflow_definition::WorkflowStep;
use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::InitFrontend;
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
use crate::engine::message::{UserMessage, UserMessageSink};
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::engine::step_status::StepStatus;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

/// Parsed flag/argument store populated from the HTTP request's `args` vector.
#[derive(Debug)]
struct ParsedArgs {
    bools: HashMap<String, bool>,
    strings: HashMap<String, String>,
    strings_vec: HashMap<String, Vec<String>>,
    paths: HashMap<String, PathBuf>,
    enums: HashMap<String, String>,
    u16s: HashMap<String, u16>,
    args: HashMap<String, String>,
    args_vec: HashMap<String, Vec<String>>,
}

/// The headless dispatch frontend. Owns a handle to the command's log file
/// for streaming output.
pub struct HeadlessDispatchFrontend {
    parsed: ParsedArgs,
    log_file: Arc<Mutex<std::fs::File>>,
}

impl HeadlessDispatchFrontend {
    /// Construct a new frontend from the HTTP request's subcommand + args.
    ///
    /// `log_path` is the `output.log` file that all output will be written to.
    /// `subcommand` is the command path (e.g. "exec prompt" → ["exec", "prompt"]).
    /// `args` is the raw args vector from the HTTP request body.
    pub fn new(subcommand: &str, args: &[String], log_path: &Path) -> Result<Self, CommandError> {
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)
            .map_err(|e| CommandError::Other(format!("Failed to open log file: {e}")))?;

        let parsed = parse_args_to_flags(subcommand, args);

        Ok(Self {
            parsed,
            log_file: Arc::new(Mutex::new(log_file)),
        })
    }

    fn write_to_log(&self, text: &str) {
        if let Ok(mut f) = self.log_file.lock() {
            let _ = writeln!(f, "{text}");
            let _ = f.flush();
        }
    }
}

/// Parse a raw args vector (CLI-style flags/positionals) into typed storage.
fn parse_args_to_flags(subcommand: &str, args: &[String]) -> ParsedArgs {
    let mut bools = HashMap::new();
    let mut strings = HashMap::new();
    let mut strings_vec: HashMap<String, Vec<String>> = HashMap::new();
    let mut paths = HashMap::new();
    let mut enums = HashMap::new();
    let mut u16s = HashMap::new();
    let mut positional_args = HashMap::new();
    let mut positional_args_vec: HashMap<String, Vec<String>> = HashMap::new();

    let mut i = 0;
    let mut positionals: Vec<String> = Vec::new();
    let mut after_double_dash = false;

    while i < args.len() {
        let arg = &args[i];

        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }

        if after_double_dash {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }

        if let Some(flag_name) = arg.strip_prefix("--") {
            if let Some((key, val)) = flag_name.split_once('=') {
                strings.insert(key.to_string(), val.to_string());
                strings_vec
                    .entry(key.to_string())
                    .or_default()
                    .push(val.to_string());
            } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                let next = &args[i + 1];
                if next == "true" || next == "false" {
                    bools.insert(flag_name.to_string(), next == "true");
                } else if let Ok(n) = next.parse::<u16>() {
                    u16s.insert(flag_name.to_string(), n);
                    strings.insert(flag_name.to_string(), next.clone());
                } else {
                    strings.insert(flag_name.to_string(), next.clone());
                    strings_vec
                        .entry(flag_name.to_string())
                        .or_default()
                        .push(next.clone());
                    enums.insert(flag_name.to_string(), next.clone());
                    paths.insert(flag_name.to_string(), PathBuf::from(next));
                }
                i += 1;
            } else {
                bools.insert(flag_name.to_string(), true);
            }
        } else {
            positionals.push(arg.clone());
        }
        i += 1;
    }

    // Map positionals to argument names based on subcommand.
    match subcommand {
        "exec prompt" => {
            if !positionals.is_empty() {
                positional_args.insert("prompt".to_string(), positionals.join(" "));
            }
        }
        "exec workflow" => {
            if let Some(wf) = positionals.first() {
                positional_args.insert("workflow".to_string(), wf.clone());
                paths.insert("workflow".to_string(), PathBuf::from(wf));
            }
        }
        "specs amend" => {
            if let Some(wi) = positionals.first() {
                positional_args.insert("work_item".to_string(), wi.clone());
            }
        }
        "config get" => {
            if let Some(f) = positionals.first() {
                positional_args.insert("field".to_string(), f.clone());
            }
        }
        "config set" => {
            if let Some(f) = positionals.first() {
                positional_args.insert("field".to_string(), f.clone());
            }
            if let Some(v) = positionals.get(1) {
                positional_args.insert("value".to_string(), v.clone());
            }
        }
        "remote run" => {
            if !positionals.is_empty() {
                positional_args_vec.insert("command".to_string(), positionals.clone());
            }
        }
        "remote session start" => {
            if let Some(d) = positionals.first() {
                positional_args.insert("dir".to_string(), d.clone());
            }
        }
        "remote session kill" => {
            if let Some(s) = positionals.first() {
                positional_args.insert("session_id".to_string(), s.clone());
            }
        }
        _ => {
            // For other commands, first positional is a generic argument.
            if let Some(first) = positionals.first() {
                positional_args.insert("prompt".to_string(), first.clone());
            }
        }
    }

    // --non-interactive is always implied for headless dispatch.
    bools.insert("non-interactive".to_string(), true);

    ParsedArgs {
        bools,
        strings,
        strings_vec,
        paths,
        enums,
        u16s,
        args: positional_args,
        args_vec: positional_args_vec,
    }
}

// ─── UserMessageSink ────────────────────────────────────────────────────────

impl UserMessageSink for HeadlessDispatchFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        let prefix = match msg.level {
            crate::engine::message::MessageLevel::Info => "[INFO]",
            crate::engine::message::MessageLevel::Warning => "[WARN]",
            crate::engine::message::MessageLevel::Error => "[ERROR]",
            crate::engine::message::MessageLevel::Success => "[OK]",
        };
        self.write_to_log(&format!("{prefix} {}", msg.text));
    }

    fn replay_queued(&mut self) {}
}

// ─── CommandFrontend (flag/argument access) ─────────────────────────────────

impl CommandFrontend for HeadlessDispatchFrontend {
    fn flag_bool(&self, _command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
        Ok(self.parsed.bools.get(flag).copied())
    }

    fn flag_string(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self.parsed.strings.get(flag).cloned())
    }

    fn flag_strings(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Vec<String>, CommandError> {
        Ok(self
            .parsed
            .strings_vec
            .get(flag)
            .cloned()
            .unwrap_or_default())
    }

    fn flag_path(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        Ok(self.parsed.paths.get(flag).cloned())
    }

    fn flag_enum(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self.parsed.enums.get(flag).cloned())
    }

    fn flag_u16(&self, _command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
        Ok(self.parsed.u16s.get(flag).copied())
    }

    fn argument(&self, _command_path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        Ok(self.parsed.args.get(name).cloned())
    }

    fn arguments(&self, _command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        Ok(self.parsed.args_vec.get(name).cloned().unwrap_or_default())
    }
}

// ─── ContainerFrontend ──────────────────────────────────────────────────────

#[async_trait]
impl ContainerFrontend for HeadlessDispatchFrontend {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        if let Ok(mut f) = self.log_file.lock() {
            let _ = f.write_all(bytes);
            let _ = f.flush();
        }
        Ok(())
    }

    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        if let Ok(mut f) = self.log_file.lock() {
            let _ = f.write_all(bytes);
            let _ = f.flush();
        }
        Ok(())
    }

    async fn read_stdin(&mut self, _buf: &mut [u8]) -> Result<usize, EngineError> {
        Ok(0) // EOF — headless has no interactive stdin
    }

    fn report_status(&mut self, status: ContainerStatus) {
        let msg = match &status {
            ContainerStatus::Building => "Container: building",
            ContainerStatus::Pulling => "Container: pulling image",
            ContainerStatus::Starting => "Container: starting",
            ContainerStatus::Running { container_name } => {
                self.write_to_log(&format!("[INFO] Container running: {container_name}"));
                return;
            }
            ContainerStatus::Stopping => "Container: stopping",
            ContainerStatus::Exited(code) => {
                self.write_to_log(&format!("[INFO] Container exited with code {code}"));
                return;
            }
            ContainerStatus::Failed(reason) => {
                self.write_to_log(&format!("[ERROR] Container failed: {reason}"));
                return;
            }
        };
        self.write_to_log(&format!("[INFO] {msg}"));
    }

    fn report_progress(&mut self, progress: ContainerProgress) {
        self.write_to_log(&format!("[INFO] {}: {}", progress.stage, progress.message));
    }

    fn resize_pty(&mut self, _cols: u16, _rows: u16) {}
}

// ─── HasContainerFrontend ───────────────────────────────────────────────────

impl HasContainerFrontend for HeadlessDispatchFrontend {
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(HeadlessContainerSink {
            log_file: Arc::clone(&self.log_file),
        })
    }
}

/// Standalone container frontend that writes to the shared log file.
struct HeadlessContainerSink {
    log_file: Arc<Mutex<std::fs::File>>,
}

impl UserMessageSink for HeadlessContainerSink {
    fn write_message(&mut self, msg: UserMessage) {
        let prefix = match msg.level {
            crate::engine::message::MessageLevel::Info => "[INFO]",
            crate::engine::message::MessageLevel::Warning => "[WARN]",
            crate::engine::message::MessageLevel::Error => "[ERROR]",
            crate::engine::message::MessageLevel::Success => "[OK]",
        };
        if let Ok(mut f) = self.log_file.lock() {
            let _ = writeln!(f, "{prefix} {}", msg.text);
            let _ = f.flush();
        }
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl ContainerFrontend for HeadlessContainerSink {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        if let Ok(mut f) = self.log_file.lock() {
            let _ = f.write_all(bytes);
            let _ = f.flush();
        }
        Ok(())
    }
    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        if let Ok(mut f) = self.log_file.lock() {
            let _ = f.write_all(bytes);
            let _ = f.flush();
        }
        Ok(())
    }
    async fn read_stdin(&mut self, _buf: &mut [u8]) -> Result<usize, EngineError> {
        Ok(0)
    }
    fn report_status(&mut self, _status: ContainerStatus) {}
    fn report_progress(&mut self, _progress: ContainerProgress) {}
    fn resize_pty(&mut self, _cols: u16, _rows: u16) {}
}

// ─── MountScopeFrontend ─────────────────────────────────────────────────────

impl MountScopeFrontend for HeadlessDispatchFrontend {
    fn ask_mount_scope(
        &mut self,
        _git_root: &Path,
        _cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        Ok(MountScopeDecision::MountGitRoot)
    }
}

// ─── AgentSetupFrontend ─────────────────────────────────────────────────────

impl AgentSetupFrontend for HeadlessDispatchFrontend {
    fn ask_agent_setup(
        &mut self,
        _requested: &AgentName,
        _default: &AgentName,
        default_available: bool,
        _image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        if default_available {
            Ok(AgentSetupDecision::Setup)
        } else {
            Ok(AgentSetupDecision::Abort)
        }
    }

    fn record_fallback(&mut self, _requested: &AgentName, _fallback: &AgentName) {}
}

// ─── AgentAuthFrontend ──────────────────────────────────────────────────────

impl AgentAuthFrontend for HeadlessDispatchFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _agent: &AgentName,
        _env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(AgentAuthDecision::Accept)
    }
}

// ─── WorkflowFrontend ───────────────────────────────────────────────────────

impl WorkflowFrontend for HeadlessDispatchFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &crate::data::workflow_state::WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if available.can_launch_next {
            Ok(NextAction::LaunchNext)
        } else {
            Ok(NextAction::Abort)
        }
    }

    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::AdvanceNow)
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.write_to_log(&format!("[INFO] Step '{}': {:?}", step.name, status));
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }

    fn user_choose_after_step_failure(
        &mut self,
        _step: &WorkflowStep,
        _exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        Ok(StepFailureChoice::Abort)
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        self.write_to_log(&format!("[INFO] Workflow completed: {outcome:?}"));
    }
}

// ─── WorktreeLifecycleFrontend ──────────────────────────────────────────────

impl WorktreeLifecycleFrontend for HeadlessDispatchFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        Ok(PreWorktreeDecision::Commit {
            message: suggested_message.to_string(),
        })
    }

    fn ask_existing_worktree(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(ExistingWorktreeDecision::Resume)
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        self.write_to_log(&format!(
            "[INFO] Worktree created: {} (branch: {branch})",
            path.display()
        ));
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &crate::command::commands::worktree_lifecycle::PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        if prompt.had_error {
            Ok(PostWorkflowWorktreeAction::Keep)
        } else {
            Ok(PostWorkflowWorktreeAction::Merge)
        }
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(Some(suggested_message.to_string()))
    }

    fn confirm_squash_merge(&mut self, _branch: &str) -> Result<bool, CommandError> {
        Ok(true)
    }

    fn confirm_worktree_cleanup(
        &mut self,
        _branch: &str,
        _path: &Path,
    ) -> Result<bool, CommandError> {
        Ok(true)
    }

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, _git_root: &Path) {
        self.write_to_log(&format!(
            "[WARN] Merge conflict on branch '{branch}' at {}",
            worktree_path.display()
        ));
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        self.write_to_log(&format!("[INFO] Worktree discarded: {branch}"));
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        self.write_to_log(&format!(
            "[INFO] Worktree kept: {} (branch: {branch})",
            path.display()
        ));
    }
}

// ─── InitFrontend ───────────────────────────────────────────────────────────

impl InitFrontend for HeadlessDispatchFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        Ok(None)
    }
    fn report_phase(&mut self, phase: &InitPhase) {
        self.write_to_log(&format!("[INFO] Init phase: {phase:?}"));
    }
    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.write_to_log(&format!("[INFO] Init step '{step}': {status:?}"));
    }
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(HeadlessContainerSink {
            log_file: Arc::clone(&self.log_file),
        })
    }
    fn report_summary(&mut self, _summary: &InitSummary) {}
}

// ─── ReadyFrontend ──────────────────────────────────────────────────────────

impl ReadyFrontend for HeadlessDispatchFrontend {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn ask_migrate_legacy_layout(&mut self, _agent_name: &AgentName) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.write_to_log(&format!("[INFO] Ready phase: {phase:?}"));
    }
    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.write_to_log(&format!("[INFO] Ready step '{step}': {status:?}"));
    }
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(HeadlessContainerSink {
            log_file: Arc::clone(&self.log_file),
        })
    }
    fn report_summary(&mut self, _summary: &ReadySummary) {}
}

// ─── Per-command frontend markers ───────────────────────────────────────────

impl RemoteCommandFrontend for HeadlessDispatchFrontend {}
impl DownloadCommandFrontend for HeadlessDispatchFrontend {}

impl StatusCommandFrontend for HeadlessDispatchFrontend {}

impl AuthCommandFrontend for HeadlessDispatchFrontend {}

impl ConfigCommandFrontend for HeadlessDispatchFrontend {
    fn present_config_table(
        &mut self,
        _rows: &[ConfigFieldRow],
    ) -> Result<Option<ConfigEditRequest>, CommandError> {
        Ok(None)
    }
}

#[async_trait]
impl HeadlessCommandFrontend for HeadlessDispatchFrontend {
    async fn serve_until_shutdown(
        &mut self,
        _config: HeadlessServeConfig,
    ) -> Result<(), CommandError> {
        Err(CommandError::Other(
            "Cannot start a nested headless server from within headless dispatch".into(),
        ))
    }
}

impl ChatCommandFrontend for HeadlessDispatchFrontend {
    fn set_pty_active(&mut self, _active: bool) {}
}

impl ExecPromptCommandFrontend for HeadlessDispatchFrontend {}

#[async_trait]
impl ExecWorkflowCommandFrontend for HeadlessDispatchFrontend {
    fn set_pty_active(&mut self, _active: bool) {}
    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.write_to_log(&format!(
            "[INFO] Workflow summary: {} completed, {} failed",
            summary.steps_completed, summary.steps_failed
        ));
    }
    fn ask_workflow_resume_or_fresh(
        &mut self,
        _workflow_name: &str,
        _completed_steps: usize,
        _total_steps: usize,
    ) -> Result<bool, CommandError> {
        // Headless mode has no interactive prompt; resume by default.
        Ok(true)
    }
}

impl SpecsCommandFrontend for HeadlessDispatchFrontend {}

impl NewCommandFrontend for HeadlessDispatchFrontend {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frontend(
        subcommand: &str,
        args: &[&str],
        tmp: &std::path::Path,
    ) -> HeadlessDispatchFrontend {
        let log_path = tmp.join("test.log");
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        HeadlessDispatchFrontend::new(subcommand, &args, &log_path).unwrap()
    }

    // ─── flag_bool ────────────────────────────────────────────────────────────

    #[test]
    fn flag_bool_bare_flag_is_true() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &["--yolo"], tmp.path());
        assert_eq!(f.flag_bool(&["chat"], "yolo").unwrap(), Some(true));
    }

    #[test]
    fn flag_bool_absent_flag_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &[], tmp.path());
        assert_eq!(f.flag_bool(&["chat"], "yolo").unwrap(), None);
    }

    #[test]
    fn flag_bool_with_explicit_true_value() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &["--background", "true"], tmp.path());
        assert_eq!(
            f.flag_bool(&["headless", "start"], "background").unwrap(),
            Some(true)
        );
    }

    #[test]
    fn flag_bool_with_explicit_false_value() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &["--background", "false"], tmp.path());
        assert_eq!(
            f.flag_bool(&["headless", "start"], "background").unwrap(),
            Some(false)
        );
    }

    // ─── flag_string ──────────────────────────────────────────────────────────

    #[test]
    fn flag_string_parses_value_after_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &["--session", "sess-123"], tmp.path());
        assert_eq!(
            f.flag_string(&["chat"], "session").unwrap().as_deref(),
            Some("sess-123")
        );
    }

    #[test]
    fn flag_string_parses_equals_syntax() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &["--session=sess-456"], tmp.path());
        assert_eq!(
            f.flag_string(&["chat"], "session").unwrap().as_deref(),
            Some("sess-456")
        );
    }

    #[test]
    fn flag_string_absent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &[], tmp.path());
        assert_eq!(f.flag_string(&["chat"], "session").unwrap(), None);
    }

    // ─── flag_u16 ─────────────────────────────────────────────────────────────

    #[test]
    fn flag_u16_parses_port_value() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("headless start", &["--port", "9876"], tmp.path());
        assert_eq!(
            f.flag_u16(&["headless", "start"], "port").unwrap(),
            Some(9876)
        );
    }

    // ─── argument (positional) ────────────────────────────────────────────────

    #[test]
    fn argument_exec_prompt_maps_positional_to_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("exec prompt", &["hello", "world"], tmp.path());
        assert_eq!(
            f.argument(&["exec", "prompt"], "prompt")
                .unwrap()
                .as_deref(),
            Some("hello world")
        );
    }

    // ─── arguments (positional vec) ───────────────────────────────────────────

    #[test]
    fn arguments_remote_run_maps_trailing_args_after_double_dash() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("remote run", &["--", "exec", "prompt", "hi"], tmp.path());
        let cmd = f.arguments(&["remote", "run"], "command").unwrap();
        assert_eq!(cmd, vec!["exec", "prompt", "hi"]);
    }

    // ─── non-interactive flag is always set ───────────────────────────────────

    #[test]
    fn non_interactive_flag_always_set() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend("chat", &[], tmp.path());
        assert_eq!(
            f.flag_bool(&["chat"], "non-interactive").unwrap(),
            Some(true),
            "non-interactive must always be set in headless mode"
        );
    }

    // ─── flag_strings (multi-value) ───────────────────────────────────────────

    #[test]
    fn flag_strings_collects_multiple_values() {
        let tmp = tempfile::tempdir().unwrap();
        let f = make_frontend(
            "headless start",
            &["--workdirs", "/a", "--workdirs", "/b"],
            tmp.path(),
        );
        let dirs = f.flag_strings(&["headless", "start"], "workdirs").unwrap();
        assert!(dirs.contains(&"/a".to_string()));
        assert!(dirs.contains(&"/b".to_string()));
    }
}
