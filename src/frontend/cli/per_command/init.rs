//! `InitFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin (when it is a TTY) for aspec replacement, audit,
//! and work-items config. The answer it falls back to on a non-TTY — which is
//! also the answer an empty line accepts on a TTY — comes from the CLI profile
//! in `src/command/headless.rs`.

use crate::data::config::repo::WorkItemsConfig;
use crate::data::prompt::Prompt;
use crate::data::step_status::StepStatus;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::{DockerfileSetupChoice, DockerfileSetupDecision};
use crate::engine::init::{InitFrontend, InitPhase, InitSummary};

use crate::frontend::cli::command_frontend::CliFrontend;

use super::helpers::{read_line, yes_no};
use crate::frontend::render_helpers::render_summary_box;

impl InitFrontend for CliFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.replace_aspec());
        }
        eprintln!();
        eprintln!("awman: The aspec/ folder contains your project specification files —");
        eprintln!("awman: architecture docs, design decisions, and work item templates.");
        eprintln!("awman: Replacing it will overwrite any customisations you've made.");
        eprintln!();
        Ok(yes_no(
            "An aspec/ folder already exists. Replace it with fresh templates?",
            self.headless.replace_aspec(),
        )
        .unwrap_or_else(|| self.headless.replace_aspec()))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.run_audit());
        }
        eprintln!();
        eprintln!("awman: The agent audit scans your repository and tailors the");
        eprintln!("awman: Dockerfile.dev for your project's language, build tools,");
        eprintln!("awman: and dependencies. It runs inside a container and does not");
        eprintln!("awman: modify your repository — only the generated Dockerfile.");
        eprintln!();
        Ok(yes_no(
            "Run the agent audit container to scan and customise the Dockerfile?",
            self.headless.run_audit(),
        )
        .unwrap_or_else(|| self.headless.run_audit()))
    }

    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.work_items_setup());
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

    /// The wording, the options and the answer a dismissal means are
    /// `prompt`'s (F-19); `display_path` is a fact the engine resolved, shown
    /// beside the question.
    fn ask_dockerfile_setup(
        &mut self,
        prompt: &Prompt<DockerfileSetupChoice>,
        git_root: &std::path::Path,
        display_path: &str,
    ) -> Result<DockerfileSetupDecision, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.dockerfile_setup());
        }
        eprintln!("awman: looked for a Dockerfile at {display_path}");
        let choice = self
            .pick_from_prompt(prompt)
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let path = if choice == DockerfileSetupChoice::UseExisting {
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
        Ok(choice.decide(prompt, path))
    }

    fn report_phase(&mut self, _phase: &InitPhase) {
        // InitPhase is an internal state-machine token; users see progress
        // through `report_step_status` and the final summary box.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::prompts;
    use crate::engine::init::frontend::DockerfileSetupDecision;

    // ─── Choice → decision, with the prompt as the only source of a default ──
    //
    // These assert the *mapping* a frontend performs, never what the answer
    // should be: the one case with no answer of its own reads it back out of
    // the prompt, so nothing here pins a default (WI 0114 F-19, Test
    // Considerations).

    #[test]
    fn create_new_maps_to_create_new() {
        let prompt = prompts::dockerfile_setup();
        assert_eq!(
            DockerfileSetupChoice::CreateNew.decide(&prompt, None),
            DockerfileSetupDecision::CreateNew
        );
    }

    #[test]
    fn use_existing_with_a_path_maps_to_that_path() {
        let prompt = prompts::dockerfile_setup();
        assert_eq!(
            DockerfileSetupChoice::UseExisting
                .decide(&prompt, Some("docker/Dockerfile".to_string())),
            DockerfileSetupDecision::UseExisting("docker/Dockerfile".to_string())
        );
    }

    #[test]
    fn skip_maps_to_skip() {
        let prompt = prompts::dockerfile_setup();
        assert_eq!(
            DockerfileSetupChoice::Skip.decide(&prompt, None),
            DockerfileSetupDecision::Skip
        );
    }

    /// Naming no path is not an answer, so the prompt's own dismissal answer
    /// applies — read from the prompt rather than written here.
    #[test]
    fn use_existing_with_an_empty_path_falls_back_to_the_prompts_answer() {
        let prompt = prompts::dockerfile_setup();
        let expected = prompt
            .default_on_dismiss
            .expect("the dockerfile prompt declares a dismissal answer")
            .decide(&prompt, None);
        assert_eq!(
            DockerfileSetupChoice::UseExisting.decide(&prompt, Some(String::new())),
            expected
        );
    }
}
