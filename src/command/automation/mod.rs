//! Automation admission, the shared execution controller and the strict
//! result policy — Layer 2 (WI 0070 Part B).
//!
//! * [`admission`] — the pure per-execution state/revision machine.
//! * [`controller`] — the process-wide execution controller: durable
//!   reservation, dedup, guarded delivery, deadlines, cancellation,
//!   finalization and crash recovery. Credential rotation builds on it.
//! * [`results`] — weakest-tier aggregation, failure precedence and the
//!   strict headless result.
//! * [`frontend`] — the launch wrapper binding an execution to its runtime
//!   I/O (OSC decoding, manual-input gate, writer, terminal registration).
//! * [`launch`] — argument admission and launch/finish glue.
//! * [`capabilities`] — capability snapshots and turn pages for routes.

pub mod admission;
pub mod capabilities;
pub mod controller;
pub mod frontend;
pub mod launch;
pub mod results;

pub use controller::{AutomationHost, ExecutionController, OwnedExecution};
