//! The WI-0092 leader/repair budget, shared by every caller that asks an agent
//! to design a `workflow.toml`.
//!
//! `exec workflow --dynamic` and the squad task evaluator both launch a
//! leader agent, validate whatever it wrote, and — on a validation failure —
//! relaunch it with a repair prompt carrying the verbatim error, up to three
//! times. That decision core is what lives here: the attempt budget, the
//! attempt label, the repair-prompt substitution, and the exhaustion message.
//!
//! What deliberately does *not* live here is how a leader is launched or
//! driven. `exec workflow --dynamic` drives an interactive container through
//! the stuck → yolo-countdown → control-board pipeline; squad runs one
//! unattended. Those are genuinely different, and folding them together would
//! buy nothing. Keeping the budget in one place is what stops the two callers
//! from disagreeing about what "3 repair attempts" means.

use std::path::{Path, PathBuf};

use crate::data::dynamic_workflow_assets::build_repair_prompt;
use crate::data::workflow_definition::Workflow;

/// What the loop decided after one leader attempt was validated.
#[derive(Debug)]
pub enum RepairDecision {
    /// The workflow validated; run it.
    Accepted(Box<Workflow>),
    /// Validation failed and budget remains: relaunch with
    /// [`WorkflowRepairLoop::prompt`], which now carries the repair prompt.
    Retry { attempt: usize, error: String },
    /// The budget is spent. The string is the final user-facing error.
    Exhausted(String),
}

/// The attempt budget for one leader/repair sequence.
pub struct WorkflowRepairLoop {
    generated_path: PathBuf,
    initial_prompt: String,
    current_prompt: String,
    attempt: usize,
}

impl WorkflowRepairLoop {
    /// The number of *repair* attempts allowed after the initial leader run.
    pub const MAX_REPAIR_ATTEMPTS: usize = 3;

    pub fn new(generated_path: impl Into<PathBuf>, initial_prompt: impl Into<String>) -> Self {
        let initial_prompt = initial_prompt.into();
        Self {
            generated_path: generated_path.into(),
            current_prompt: initial_prompt.clone(),
            initial_prompt,
            attempt: 0,
        }
    }

    /// The prompt for the next launch: the original leader prompt on attempt 0,
    /// a repair prompt carrying the last validation error afterwards.
    pub fn prompt(&self) -> &str {
        &self.current_prompt
    }

    /// How many repair attempts have been consumed so far.
    pub fn attempt(&self) -> usize {
        self.attempt
    }

    /// Whether the next launch is the first (original-prompt) one.
    pub fn is_first_attempt(&self) -> bool {
        self.attempt == 0
    }

    /// The label for the next launch, used for container/step naming.
    pub fn label(&self) -> String {
        if self.attempt == 0 {
            "leader".to_string()
        } else {
            format!("leader-repair-{}", self.attempt)
        }
    }

    /// Where the leader is expected to write its workflow.
    pub fn generated_path(&self) -> &Path {
        &self.generated_path
    }

    /// Discard any partial output and restart from the original prompt with a
    /// fresh budget. A user-requested restart is not a validation failure.
    pub fn restart(&mut self) {
        let _ = std::fs::remove_file(&self.generated_path);
        self.attempt = 0;
        self.current_prompt = self.initial_prompt.clone();
    }

    /// Record one attempt's validation result and decide what happens next.
    pub fn record(&mut self, validation: Result<Workflow, String>) -> RepairDecision {
        match validation {
            Ok(workflow) => RepairDecision::Accepted(Box::new(workflow)),
            Err(error) => {
                self.attempt += 1;
                if self.attempt > Self::MAX_REPAIR_ATTEMPTS {
                    return RepairDecision::Exhausted(format!(
                        "leader agent failed to produce a valid workflow.toml after {} repair \
                         attempts; last error: {error}; file is at {}",
                        Self::MAX_REPAIR_ATTEMPTS,
                        self.generated_path.display()
                    ));
                }
                self.current_prompt = build_repair_prompt(&error);
                RepairDecision::Retry {
                    attempt: self.attempt,
                    error,
                }
            }
        }
    }
}
