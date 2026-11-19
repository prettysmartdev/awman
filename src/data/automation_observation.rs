//! Typed lifecycle observations — Layer 0 (WI 0070).
//!
//! Layer 1 produces these from exactly two sources: the execution-private
//! native lifecycle channel (awman-authored guest hooks writing versioned
//! JSONL) and decoded OSC 7501 reports from the existing scanner. Layer 2's
//! admission state machine consumes nothing else: no screen cells, rendered
//! text, silence timers or other ANSI patterns ever become an observation.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::data::ids::ExecutionId;
use crate::data::settings_profile::NativeSessionId;

static RECEIVE_CLOCK: AtomicU64 = AtomicU64::new(0);

/// The next value of the process-wide monotone host receive clock. Every
/// producer stamps an observation when it decodes the bytes it just
/// received, and admission takes a stamp at each invalidation boundary
/// (manual input, control change, acceptance, update). An observation
/// received before a boundary therefore never establishes readiness after
/// it, whatever order the controller applies decoded batches in.
pub fn next_receive_sequence() -> u64 {
    RECEIVE_CLOCK.fetch_add(1, Ordering::SeqCst) + 1
}

/// Which lifecycle source produced an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObservationSource {
    Native,
    Osc7501,
}

/// Where an observation belongs: the owned execution, the source's channel
/// generation (a reset/replacement starts a new one) and the host's
/// monotone receive sequence. Admission uses host receive order only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationBinding {
    pub execution_id: ExecutionId,
    pub source: ObservationSource,
    pub generation: u64,
    pub sequence: u64,
}

/// Why a blocked provider is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockedReason {
    Auth,
    Permission,
    Input,
}

/// A settled native user-turn outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeOutcome {
    /// Confirmed success. `final_message` may be `Some("")`: an explicitly
    /// empty native answer is still a confirmed answer.
    Succeeded { final_message: Option<String> },
    /// Confirmed failure (still a confirmed settlement).
    Failed { message: String },
}

/// One normalized lifecycle observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationKind {
    /// The provider's own session identity record for the root session.
    Identity {
        native_session_id: NativeSessionId,
    },
    /// Facility evidence about the native lifecycle contract on this route:
    /// `Some(true)` the complete contract (identity, acceptance, blocked,
    /// continuation, settlement and input readiness) is present; `Some(false)`
    /// actual evidence establishes it is absent; with a concrete reason.
    Facility {
        full_lifecycle: Option<bool>,
        reason: Option<String>,
    },
    /// The native contract says the exact session accepts a task with no
    /// pending tool, login, approval or turn. Never inferred from a session
    /// start or a generic stop.
    Ready,
    /// The provider accepted the outstanding submission. `prompt_digest`
    /// correlates it with the delivered prompt where the contract echoes it.
    PromptAccepted {
        native_turn_id: Option<String>,
        prompt_digest: Option<String>,
    },
    /// Activity: work is in progress (including session-start/stop hooks,
    /// which are activity only).
    Working,
    Blocked(BlockedReason),
    /// A stop that the provider continues (tool request, continuation hook):
    /// the turn remains active and no answer is final.
    Continuation,
    /// A correlated settled user-turn outcome through its output boundary.
    Settled {
        native_turn_id: Option<String>,
        outcome: NativeOutcome,
    },
    /// The source failed, overflowed, gapped or ended. Loss is not idle.
    SourceLost {
        reason: String,
    },
    /// An authoritative native resynchronization: no pending turn when
    /// `active` is false, and the known blocked condition if any.
    Resynchronized {
        active: bool,
        blocked: Option<BlockedReason>,
    },
    /// A malformed recognized record, impossible turn ID, contradictory
    /// required records or completion before acceptance.
    ProtocolFailure {
        reason: String,
    },
    /// OSC root `idle`.
    OscIdle,
    /// OSC root `working`.
    OscWorking,
    /// OSC root `done`.
    OscDone,
    /// OSC root `error`.
    OscError,
    /// A descendant (id-present) report holding working/blocked/error: a
    /// veto on root readiness, never root evidence. `false` clears it.
    OscDescendantVeto(bool),
    /// Clear/RIS/stream-end or descendant overflow: OSC readiness and active
    /// inference are invalidated.
    OscInvalidated {
        reason: String,
    },
}

/// One observation with its binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub binding: ObservationBinding,
    pub kind: ObservationKind,
}
