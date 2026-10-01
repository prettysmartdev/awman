//! `ReadyFrontend` impl for the CLI.
//!
//! Prompts on stdin for the Dockerfile creation decision when stdin is a TTY;
//! otherwise returns the CLI profile's answers from
//! `src/command/headless.rs`.

use crate::engine::error::EngineError;
use crate::engine::ready::{ReadyFrontend, ReadyPhase, ReadySummary};

use crate::frontend::cli::command_frontend::CliFrontend;

use super::helpers::yes_no;

impl ReadyFrontend for CliFrontend {
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.create_dockerfile());
        }
        Ok(yes_no(
            &format!(
                "No Dockerfile found at {}. Create one from the default template?",
                dockerfile_path.display()
            ),
            self.headless.create_dockerfile(),
        )
        .unwrap_or_else(|| self.headless.create_dockerfile()))
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.run_audit_on_template());
        }
        Ok(yes_no(
            "Run the agent audit container to scan and customise the Dockerfile?",
            self.headless.run_audit_on_template(),
        )
        .unwrap_or_else(|| self.headless.run_audit_on_template()))
    }

    fn report_phase(&mut self, _phase: &ReadyPhase) {
        // The ReadyPhase enum is an internal state-machine token; users see
        // progress through `report_step_status` and the final summary box.
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        // When --json is active, suppress the human-readable summary box on
        // stderr — only the JSON output on stdout matters.
        if self.is_json_mode() {
            return;
        }
        // The rows, outcome line and hint are the summary's own (F-23;
        // readiness now accounts for per-image build/import results too):
        // this renders them without recomputing any of that itself.
        let box_str = crate::frontend::render_helpers::render_ready_summary(summary);
        let outcome_line = summary.outcome_line();
        // Write the summary box directly to stderr without the per-line
        // "awman:" prefix used for status updates — the box is multi-line
        // content that reads better unprefixed.
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("\n{box_str}{outcome_line}\n").as_bytes(),
        );

        if let Some(hint) = summary.hint() {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("Tip: {hint}\n").as_bytes(),
            );
        }
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}
