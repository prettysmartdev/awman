//! squad — the always-on task scheduler (Layer 1).
//!
//! This module owns the *engine* half of squad's machinery:
//!
//! * [`SquadDaemonEngine`] — the daemon runtime itself: database relocation,
//!   store open and migrate, orphaned-run reconciliation, the stray-container
//!   scan, the scheduler, and the endpoint sidecar. Layer 3 adds only a
//!   router and a socket (WI 0113 F-02).
//! * [`SquadSupervisor`] — daemon lifecycle for *clients*: is one running,
//!   what does this process authenticate with, and start one on demand.
//! * [`SquadScheduler`] — the 30s tick loop that selects due tasks
//!   (wholly in SQL, via [`TaskStore::due_for_evaluation`]), dispatches
//!   each onto a bounded task set, records run rows, and grows an exponential
//!   backoff for repeatedly-failing tasks.
//! * [`TaskEvaluator`] — the delegation seam. The scheduler calls it for
//!   every due task; the concrete implementation lives one layer up
//!   (`command::commands::squad`) because evaluating a task needs
//!   workflow validation, the WI-0092 repair loop, and workflow execution —
//!   all Layer 2 concerns. Layer 1 only ever calls the trait (Tenet 1).
//! * [`SquadAgentLauncher`] — the genuinely Layer 1 work of launching a leader
//!   agent in a container: seeding the task directory, resolving agent
//!   options, attaching squad's two container labels, and running through
//!   `Arc<dyn AgentRuntimeEngine>`.
//!
//! [`TaskStore::due_for_evaluation`]: crate::data::fs::TaskStore::due_for_evaluation

pub mod daemon;
pub mod env_state;
pub mod env_store;
pub mod evaluator;
pub mod key_setup;
pub mod launcher;
pub mod scheduler;
pub mod supervisor;
pub mod verdict;

pub use daemon::{SquadDaemonDeps, SquadDaemonEngine};
pub use evaluator::{
    EvaluationOutcome, EvaluationRequest, NoRunProgress, RunProgress, TaskEvaluator,
};
pub use launcher::{
    drive_unattended_agent, ensure_directory_workspace_project, LeaderExit, LeaderRunSpec,
    SquadAgentLauncher, UnattendedExit,
};
pub use scheduler::{SchedulerStatus, SquadScheduler, TICK_INTERVAL};
pub use supervisor::{SquadEndpoint, SquadKeyState, SquadSupervisor};
pub use verdict::{
    read_verdict, verdict_path, RunVerdict, VerdictError, RUN_DIR_CONTAINER_PATH, VERDICT_FILE_NAME,
};
