//! `ExecWorkflowCommandFrontend` impl for the CLI.
//!
//! All supertraits (`UserMessageSink`, `AgentFrontend`, `WorkflowFrontend`,
//! `MountScopeFrontend`, `AgentSetupFrontend`, `AgentAuthFrontend`,
//! `WorktreeLifecycleFrontend`) are implemented elsewhere in
//! `src/frontend/cli/`; this file only carries the trait's own methods
//! (PTY gating, the summary, and the resume prompts).

use crate::command::commands::exec_workflow::{
    ExecWorkflowCommandFrontend, WorkflowResumeDecision, WorkflowResumePrompt, WorkflowSummary,
};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::workflow_state::PhaseKind;

use crate::frontend::cli::command_frontend::CliFrontend;

impl ExecWorkflowCommandFrontend for CliFrontend {
    /// Take the terminal back from the exited container: stop the raw stdin
    /// reader and restore cooked mode before the step's result is printed.
    /// Agent steps get the same release from their terminal
    /// `report_step_status`, which a phase step never receives.
    fn report_phase_step_container_exited(&mut self, _kind: PhaseKind, _exit_code: i32) {
        self.unbind_container_stdio();
        self.container_stdin_tx = None;
    }

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "workflow summary — {}/{} steps OK ({} failed)",
                summary.steps_completed,
                summary.steps_completed + summary.steps_failed,
                summary.steps_failed
            ),
        });
    }

    fn ask_workflow_resume(
        &mut self,
        prompt: &WorkflowResumePrompt,
    ) -> Result<WorkflowResumeDecision, CommandError> {
        if self.non_interactive {
            return Ok(self.headless.workflow_resume(prompt));
        }
        eprintln!("awman: {}", prompt.title);
        for line in prompt.body.lines() {
            eprintln!("  {line}");
        }
        for (i, label) in prompt.choice_labels().iter().enumerate() {
            eprintln!("  [{}] {label}", i + 1);
        }
        eprintln!("  [f] {}", prompt.fresh_label);
        eprintln!("  [q] Cancel — leave the saved run alone and do nothing");

        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(WorkflowResumeDecision::Cancel);
        }
        Ok(parse_resume_answer(&buf, prompt))
    }

    fn notify_dynamic_workflow_resume_unavailable(
        &mut self,
        work_item: u32,
        reason: &str,
    ) -> Result<(), CommandError> {
        eprintln!(
            "awman: the worktree for work item {work_item:04} is still on disk, but the previous \
             dynamic workflow cannot be resumed: {reason}"
        );
        if self.non_interactive {
            return Ok(());
        }
        eprintln!("awman: press Enter to start a fresh dynamic workflow.");
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
        Ok(())
    }
}

/// Map a typed line to a resume decision.
///
/// Only an explicit `f` discards the previous run — that deletes progress
/// which cannot be got back, and in dynamic mode a leader design with it — so
/// everything ambiguous (an empty line, EOF, a typo, a number nobody offered)
/// cancels the command instead, leaving the saved run exactly as it was.
fn parse_resume_answer(input: &str, prompt: &WorkflowResumePrompt) -> WorkflowResumeDecision {
    let answer = input.trim();
    if answer.eq_ignore_ascii_case("f") {
        return WorkflowResumeDecision::Fresh;
    }
    answer
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
        .and_then(|n| prompt.start_points.get(n - 1))
        .map(|p| WorkflowResumeDecision::ResumeFrom(p.name.clone()))
        .unwrap_or(WorkflowResumeDecision::Cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::exec_workflow::WorkflowResumeStep;

    fn prompt() -> WorkflowResumePrompt {
        WorkflowResumePrompt::new(
            "wf".into(),
            None,
            None,
            false,
            1,
            3,
            vec![
                WorkflowResumeStep {
                    name: "b".into(),
                    role: "the step that failed".into(),
                },
                WorkflowResumeStep {
                    name: "a".into(),
                    role: "the step before it".into(),
                },
            ],
        )
    }

    #[test]
    fn a_number_picks_that_start_point() {
        assert_eq!(
            parse_resume_answer("2\n", &prompt()),
            WorkflowResumeDecision::ResumeFrom("a".into())
        );
    }

    #[test]
    fn only_an_explicit_f_discards_the_saved_run() {
        for answer in ["f", "F", " f \n"] {
            assert_eq!(
                parse_resume_answer(answer, &prompt()),
                WorkflowResumeDecision::Fresh,
                "answer={answer:?}"
            );
        }
    }

    /// Nothing ambiguous may be read as "delete the previous run".
    #[test]
    fn anything_unrecognised_cancels_rather_than_discarding() {
        for answer in ["", "\n", "  ", "0", "9", "yes", "fresh"] {
            assert_eq!(
                parse_resume_answer(answer, &prompt()),
                WorkflowResumeDecision::Cancel,
                "answer={answer:?}"
            );
        }
    }
}
