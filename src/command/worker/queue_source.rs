pub use crate::data::worker::queue::*;
use std::path::PathBuf;
use std::sync::Arc;

/// Implemented by the Layer 2 worker controller. Methods return quickly;
/// `pick` and `release` may block for one round trip.
pub trait QueueSource: Send + Sync {
    fn terminal_input_for_lease(&self, _lease_id: &str) -> bool {
        false
    }
    fn begin_lease(&self, _lease_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn prepared_run(&self, _lease_id: &str) -> Option<PickRun> {
        None
    }
    fn snapshot(&self) -> QueueSnapshot;
    /// Changes whenever the snapshot does.
    fn generation(&self) -> u64;
    /// The panel was opened: clear "new since seen".
    fn mark_seen(&self);
    /// Ask for a fresh listing (the next snapshot reflects it).
    fn refresh(&self);
    fn pick(&self, server: &str, item_id: &str) -> Result<PickRun, PickError>;
    fn checkout_offer(
        &self,
        _lease_id: &str,
    ) -> Option<crate::data::prompt::Prompt<CheckoutChoice>> {
        None
    }
    fn choose_checkout(
        &self,
        _lease_id: &str,
        _choice: CheckoutChoice,
    ) -> Result<PickRun, PickError> {
        Err(PickError::Failed(
            "checkout selection is unavailable".into(),
        ))
    }
    /// `Release` or `Cancel` for the lease a closing tab holds.
    fn finish_lease(&self, lease_id: &str, choice: LeaseCloseChoice) -> Result<(), String>;
    /// The prepared Layer 0 session, including its private inline source.
    fn session_for_lease(&self, _lease_id: &str) -> Option<crate::data::session::Session> {
        None
    }
    /// The caller's Dispatch run publishes into this same ordered event bus.
    fn events_for_lease(
        &self,
        _lease_id: &str,
    ) -> Option<Arc<crate::command::commands::api_server::event_bus::EventBus>> {
        None
    }
    fn cancellation_for_lease(
        &self,
        _lease_id: &str,
    ) -> Option<tokio_util::sync::CancellationToken> {
        None
    }
    fn output_dir_for_lease(&self, _lease_id: &str) -> Option<PathBuf> {
        None
    }
    fn engines_for_lease(&self, _lease_id: &str) -> Option<crate::command::dispatch::Engines> {
        None
    }
    fn writer_for_lease(
        &self,
        _lease_id: &str,
    ) -> Option<crate::data::fs::api_command_log::CommandStateWriter> {
        None
    }
    /// Finish a foreground run, applying the lease's push and report policy.
    fn complete_lease(
        &self,
        _lease_id: &str,
        _result: crate::data::fs::api_db::CommandResult,
    ) -> Result<crate::data::fs::api_db::CommandResult, String> {
        Err("the queue source cannot complete a foreground run".into())
    }
}

pub type SharedQueueSource = Arc<dyn QueueSource>;

pub fn checkout_prompt(title: &str) -> crate::data::prompt::Prompt<CheckoutChoice> {
    use crate::data::prompt::{Choice, Prompt};
    Prompt::new(
        "Run in your checkout?",
        format!("{title} belongs to this repository."),
        vec![
            Choice::new(
                'w',
                "Run in a worktree of this checkout",
                CheckoutChoice::Worktree,
            ),
            Choice::new('f', "Run in a fresh clone", CheckoutChoice::FreshClone),
        ],
        Some(CheckoutChoice::FreshClone),
    )
}

pub fn close_prompt(title: &str) -> crate::data::prompt::Prompt<LeaseCloseChoice> {
    use crate::data::prompt::{Choice, Prompt};
    Prompt::new(
        "Close running work item?",
        title,
        vec![
            Choice::new('r', "Release back to the queue", LeaseCloseChoice::Release),
            Choice::new('c', "Cancel this run", LeaseCloseChoice::Cancel),
            Choice::new('k', "Keep running", LeaseCloseChoice::KeepRunning),
        ],
        Some(LeaseCloseChoice::KeepRunning),
    )
}
