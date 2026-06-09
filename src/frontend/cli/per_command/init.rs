//! `InitFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin (when it is a TTY) for aspec replacement,
//! audit, and work-items config; otherwise it returns the safe
//! non-interactive defaults.

use crate::data::config::repo::WorkItemsConfig;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::DockerfileSetupDecision;
use crate::engine::init::{InitFrontend, InitPhase, InitSummary};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

use super::helpers::{pick_numbered, read_line, render_summary_box, step_status_label, yes_no};

impl InitFrontend for CliFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        eprintln!();
        eprintln!("awman: The aspec/ folder contains your project specification files —");
        eprintln!("awman: architecture docs, design decisions, and work item templates.");
        eprintln!("awman: Replacing it will overwrite any customisations you've made.");
        eprintln!();
        Ok(yes_no(
            "An aspec/ folder already exists. Replace it with fresh templates?",
            false,
        ))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        eprintln!();
        eprintln!("awman: The agent audit scans your repository and tailors the");
        eprintln!("awman: Dockerfile.dev for your project's language, build tools,");
        eprintln!("awman: and dependencies. It runs inside a container and does not");
        eprintln!("awman: modify your repository — only the generated Dockerfile.");
        eprintln!();
        Ok(yes_no(
            "Run the agent audit container to scan and customise the Dockerfile?",
            false,
        ))
    }

    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        if !stdin_is_tty() {
            return Ok(None);
        }
        eprintln!(
            "awman: Configure a work items directory? (path relative to repo root, empty to skip)"
        );
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(None);
        }
        let dir = buf.trim();
        if dir.is_empty() {
            return Ok(None);
        }
        eprintln!("awman: Work item template path (empty for none):");
        let mut buf2 = String::new();
        let _ = std::io::stdin().read_line(&mut buf2);
        let template_str = buf2.trim();
        let template = if template_str.is_empty() {
            None
        } else {
            Some(template_str.to_string())
        };
        Ok(Some(WorkItemsConfig {
            dir: Some(dir.to_string()),
            template,
        }))
    }

    fn ask_dockerfile_setup(
        &mut self,
        git_root: &std::path::Path,
    ) -> Result<DockerfileSetupDecision, EngineError> {
        if !stdin_is_tty() {
            return Ok(DockerfileSetupDecision::CreateNew);
        }
        let repo_cfg = crate::data::config::repo::RepoConfig::load(git_root).unwrap_or_default();
        let display_path = repo_cfg.dockerfile.as_deref().unwrap_or("Dockerfile.dev");
        let choice = pick_numbered(
            &format!("No Dockerfile found at {display_path}. How would you like to proceed?"),
            &[
                "Create Dockerfile.dev from the built-in template (recommended)",
                "Use an existing Dockerfile in this repo",
                "Skip for now (configure manually in .awman/config.json)",
            ],
            1,
        );
        let path = if choice == 2 {
            read_line("Enter the path to your Dockerfile (relative to repo root):").map(|p| {
                if !p.is_empty() {
                    let resolved = git_root.join(&p);
                    if !resolved.exists() {
                        eprintln!("awman: warning: {p} does not exist yet; saving anyway.");
                    }
                }
                p
            })
        } else {
            None
        };
        Ok(dockerfile_decision_from_input(true, choice, path))
    }

    fn report_phase(&mut self, _phase: &InitPhase) {
        // InitPhase is an internal state-machine token; users see progress
        // through `report_step_status` and the final summary box.
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        let level = match status {
            StepStatus::Failed(_) => MessageLevel::Error,
            _ => MessageLevel::Info,
        };
        self.messages.write_message(UserMessage {
            level,
            text: format!("{step}: {}", step_status_label(&status)),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }

    fn report_summary(&mut self, summary: &InitSummary) {
        let rows: Vec<(&str, &StepStatus)> = vec![
            ("Config", &summary.config),
            ("aspec folder", &summary.aspec_folder),
            ("Dockerfile.dev", &summary.dockerfile),
            ("Agent audit", &summary.audit),
            ("Docker image", &summary.image_build),
            ("Work items", &summary.work_items_setup),
        ];
        let box_str = render_summary_box("Init Summary", &rows);
        let footer = "\nWhat's Next?\n  Run `awman` to launch the interactive TUI.\n\n  Available commands:\n    awman chat          — Start a freeform chat session with the agent\n    awman new spec      — Create a new work item from the aspec template\n    awman exec workflow — Run a workflow inside a container\n";
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("\n{box_str}{footer}").as_bytes(),
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

/// Pure decision logic for `ask_dockerfile_setup`, factored out for unit-testability.
///
/// `is_tty` — whether stdin is a TTY; `choice` — the 1-based numbered choice
/// returned by `pick_numbered`; `path` — the Dockerfile path the user entered
/// (only relevant when `choice == 2`).
pub(crate) fn dockerfile_decision_from_input(
    is_tty: bool,
    choice: usize,
    path: Option<String>,
) -> DockerfileSetupDecision {
    if !is_tty {
        return DockerfileSetupDecision::CreateNew;
    }
    match choice {
        2 => match path {
            Some(p) if !p.is_empty() => DockerfileSetupDecision::UseExisting(p),
            _ => DockerfileSetupDecision::CreateNew,
        },
        3 => DockerfileSetupDecision::Skip,
        _ => DockerfileSetupDecision::CreateNew,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::engine::init::frontend::DockerfileSetupDecision;
    use crate::engine::init::InitFrontend;

    // ─── Non-TTY path ────────────────────────────────────────────────────────

    #[test]
    fn non_tty_stdin_returns_create_new() {
        // Test environments never have a TTY attached to stdin, so
        // ask_dockerfile_setup must return CreateNew immediately.
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "init"]).unwrap();
        let mut frontend = CliFrontend::new(m);
        let result = frontend
            .ask_dockerfile_setup(std::path::Path::new("/tmp"))
            .unwrap();
        assert_eq!(result, DockerfileSetupDecision::CreateNew);
    }

    // ─── TTY decision logic (via injectable helper) ───────────────────────────

    #[test]
    fn tty_choice_1_returns_create_new() {
        let result = dockerfile_decision_from_input(true, 1, None);
        assert_eq!(result, DockerfileSetupDecision::CreateNew);
    }

    #[test]
    fn tty_choice_2_with_path_returns_use_existing() {
        let result = dockerfile_decision_from_input(true, 2, Some("docker/Dockerfile".to_string()));
        assert_eq!(
            result,
            DockerfileSetupDecision::UseExisting("docker/Dockerfile".to_string())
        );
    }

    #[test]
    fn tty_choice_3_returns_skip() {
        let result = dockerfile_decision_from_input(true, 3, None);
        assert_eq!(result, DockerfileSetupDecision::Skip);
    }

    #[test]
    fn tty_choice_2_with_empty_path_falls_back_to_create_new() {
        let result = dockerfile_decision_from_input(true, 2, Some(String::new()));
        assert_eq!(result, DockerfileSetupDecision::CreateNew);
    }
}
