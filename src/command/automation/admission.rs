//! The Layer 2 admission state machine (WI 0070 B2–B4).
//!
//! One [`AdmissionCell`] per execution. It consumes only typed observations
//! from Layer 1 (native lifecycle records and decoded OSC 7501 reports) and
//! explicit controller stimuli (submission, writer ingress, manual input,
//! control change, close, cancel, deadlines, exit). It never sees screen
//! cells, rendered text or silence. It performs no I/O: the controller
//! persists each [`Effect`] and publishes the snapshot.
//!
//! Every visible change bumps `revision` exactly once per step (the store's
//! CAS requires `+1`), and every readiness is consumed by acceptance or
//! invalidated by a tier change, manual input, control change, source loss
//! or turn completion: a turn result is never itself readiness.

use crate::data::automation::{
    AutomationState, CompletionSource, MinimumStatusSupport, OutputStatus, ReadinessSource,
    StatusSource, StatusSupport, TurnStatus,
};
use crate::data::automation_observation::{
    BlockedReason, NativeOutcome, Observation, ObservationKind, ObservationSource,
};
use crate::data::automation_records::{AutomationFailure, AutomationSnapshot, TurnResult};
use crate::data::ids::{ExecutionId, TurnId};
use crate::data::settings_profile::NativeSessionId;

/// Stable refusal/failure codes (see `data::agent_failure`).
pub mod codes {
    pub const STALE_EXECUTION: &str = "stale_execution";
    pub const STALE_REVISION: &str = "stale_revision";
    pub const INPUT_NOT_ALLOWED: &str = "input_not_allowed";
    pub const CONTROL_HELD: &str = "control_held";
    pub const STATUS_SUPPORT_INSUFFICIENT: &str = "status_support_insufficient";
    pub const NATIVE_CAPABILITY_UNAVAILABLE: &str = "native_capability_unavailable";
    pub const AGENT_NOT_READY: &str = "agent_not_ready";
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const INPUT_BACKPRESSURE: &str = "input_backpressure";
    pub const IDEMPOTENCY_CONFLICT: &str = "idempotency_conflict";
    pub const STARTUP_TIMEOUT: &str = "startup_timeout";
    pub const DELIVERY_UNKNOWN: &str = "delivery_unknown";
    pub const AGENT_TURN_FAILED: &str = "agent_turn_failed";
    pub const AGENT_PROTOCOL_ERROR: &str = "agent_protocol_error";
    pub const RUNTIME_WAIT_FAILED: &str = "runtime_wait_failed";
    pub const OUTPUT_INCOMPLETE: &str = "output_incomplete";
    pub const CLEANUP_TIMEOUT: &str = "cleanup_timeout";
    pub const EXECUTION_LOST: &str = "execution_lost";
    pub const NATIVE_SESSION_NOT_FOUND: &str = "native_session_not_found";
    pub const CANCELLED: &str = "cancelled";
}

/// A typed refusal: a stable code plus a human explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn failure(&self) -> AutomationFailure {
        failure(self.code, &self.message)
    }
}

pub fn failure(code: &str, message: &str) -> AutomationFailure {
    AutomationFailure {
        code: code.into(),
        message: message.into(),
    }
}

/// What the controller must do after a step.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// The snapshot changed (revision bumped by one): persist it with the
    /// previous revision as the CAS value, then publish `automation_state`.
    State,
    /// The native identity was observed for the first time.
    Identity(NativeSessionId),
    /// The outstanding turn was acknowledged (`agent_turn_started`).
    TurnStarted {
        turn_id: TurnId,
        native_turn_id: Option<String>,
        source: StatusSource,
    },
    /// The outstanding turn settled; persist the immutable result.
    TurnCompleted { turn_id: TurnId, result: TurnResult },
    /// Startup readiness met the launch floor (startup deadline ends).
    StartupSatisfied,
    /// Stop the execution with this primary failure.
    Stop(AutomationFailure),
}

/// Immutable per-launch admission configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionConfig {
    pub minimum: MinimumStatusSupport,
    /// `agents.programStatus.enabled` resolved for this launch.
    pub osc_enabled: bool,
    /// The adapter provides the native guest lifecycle channel.
    pub native_channel: bool,
    /// The adapter has an established task delivery operation.
    pub delivery: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Veto {
    Working,
    Continuation,
    Blocked(BlockedReason),
}

#[derive(Debug, Clone, Default)]
struct NativeTrack {
    facility: Option<bool>,
    facility_reason: Option<String>,
    healthy: bool,
    ready: bool,
    veto: Option<Veto>,
    lost: Option<String>,
    /// The newest channel generation applied; older generations are stale.
    generation: u64,
    /// The last applied host receive sequence of this source.
    last_sequence: u64,
}

#[derive(Debug, Clone, Default)]
struct OscTrack {
    /// A root report has established the source on this stream.
    seen: bool,
    idle: bool,
    working_after_delivery: bool,
    blocked: Option<BlockedReason>,
    error: bool,
    descendant_veto: bool,
    generation: u64,
    last_sequence: u64,
}

/// A turn's delivery phase (mirrors `DeliveryPhase` before `Terminal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TurnPhase {
    Reserved,
    WriteStarted,
    Written,
    Acknowledged,
}

#[derive(Debug, Clone)]
struct ActiveTurn {
    turn_id: TurnId,
    phase: TurnPhase,
    weakest: StatusSupport,
    readiness_source: ReadinessSource,
    native_turn_id: Option<String>,
    prompt_digest: Option<String>,
    grant_epoch: Option<u64>,
    delivered_osc_generation: u64,
    /// The host receive stamp taken when the write was authorized: only
    /// an observation received after it can acknowledge this delivery.
    delivered_after: u64,
}

/// One execution's serialized admission lifecycle.
#[derive(Debug, Clone)]
pub struct AdmissionCell {
    snapshot: AutomationSnapshot,
    config: AdmissionConfig,
    requested_native: Option<NativeSessionId>,
    native: NativeTrack,
    osc: OscTrack,
    turn: Option<ActiveTurn>,
    stopping: bool,
    close_pending: bool,
    update: Option<AutomationState>,
    protocol_failed: bool,
    startup_satisfied: bool,
    ended: bool,
    grant_epoch: Option<u64>,
    /// The weakest tier ever established after startup (for a command
    /// with no accepted turns).
    established: StatusSupport,
    /// The host receive stamp of the last readiness invalidation
    /// (acceptance, manual input, control change, update). Observations
    /// received before it cannot establish readiness.
    readiness_watermark: u64,
}

impl AdmissionCell {
    /// A new execution: `starting`, revision 0, no source.
    pub fn new(
        mut snapshot: AutomationSnapshot,
        config: AdmissionConfig,
        requested_native: Option<NativeSessionId>,
    ) -> Self {
        snapshot.state = AutomationState::Starting;
        // Launch revision starts at 0 (B3).
        snapshot.revision = 0;
        snapshot.status_support = StatusSupport::None;
        snapshot.status_source = StatusSource::None;
        snapshot.active_turn_id = None;
        let mut cell = Self {
            snapshot,
            config,
            requested_native,
            native: NativeTrack {
                healthy: config.native_channel,
                ..Default::default()
            },
            osc: OscTrack::default(),
            turn: None,
            stopping: false,
            close_pending: false,
            update: None,
            protocol_failed: false,
            startup_satisfied: false,
            ended: false,
            grant_epoch: None,
            established: StatusSupport::None,
            readiness_watermark: 0,
        };
        cell.snapshot.status_reason = Some(cell.tier_reason());
        cell
    }

    pub fn snapshot(&self) -> &AutomationSnapshot {
        &self.snapshot
    }
    pub fn execution_id(&self) -> &ExecutionId {
        &self.snapshot.execution_id
    }
    pub fn revision(&self) -> u64 {
        self.snapshot.revision
    }
    pub fn startup_satisfied(&self) -> bool {
        self.startup_satisfied
    }
    pub fn is_stopping(&self) -> bool {
        self.stopping || self.ended
    }
    pub fn active_turn(&self) -> Option<(&TurnId, TurnPhase)> {
        self.turn.as_ref().map(|t| (&t.turn_id, t.phase))
    }
    pub fn established_support(&self) -> StatusSupport {
        self.established
    }
    pub fn close_pending(&self) -> bool {
        self.close_pending
    }

    // ── Tier and state derivation ──────────────────────────────────────

    fn native_full(&self) -> bool {
        self.config.native_channel
            && self.native.facility == Some(true)
            && self.native.healthy
            && !self.protocol_failed
    }

    fn osc_usable(&self) -> bool {
        self.config.osc_enabled && self.config.delivery && self.osc.seen
    }

    fn tier(&self) -> (StatusSupport, StatusSource) {
        if self.native_full() {
            (StatusSupport::Full, StatusSource::Native)
        } else if self.osc_usable() {
            (StatusSupport::BestEffort, StatusSource::Osc7501)
        } else {
            (StatusSupport::None, StatusSource::None)
        }
    }

    fn tier_reason(&self) -> String {
        if self.native_full() {
            return "native lifecycle contract established".into();
        }
        let native = if !self.config.native_channel {
            "the adapter has no native lifecycle source".to_string()
        } else if let Some(lost) = &self.native.lost {
            format!("native lifecycle source lost: {lost}")
        } else if self.native.facility == Some(false) {
            format!(
                "native lifecycle facility unavailable{}",
                self.native
                    .facility_reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            )
        } else {
            "native lifecycle not yet established".into()
        };
        let osc = if !self.config.osc_enabled {
            "agents.programStatus.enabled=false disables OSC 7501"
        } else if !self.config.delivery {
            "no established task delivery"
        } else if self.osc.seen {
            "using OSC 7501 reports"
        } else {
            "no OSC 7501 report received"
        };
        format!("{native}; {osc}")
    }

    /// Apply the current tier; a change invalidates readiness and lowers
    /// the active turn's weakest tier.
    fn retier(&mut self) {
        let (support, source) = self.tier();
        let reason = Some(self.tier_reason());
        if (support, source) != (self.snapshot.status_support, self.snapshot.status_source) {
            self.snapshot.status_support = support;
            self.snapshot.status_source = source;
            self.native.ready = false;
            self.osc.idle = false;
            if let Some(turn) = &mut self.turn {
                turn.weakest = turn.weakest.weakest(support);
            }
        }
        self.snapshot.status_reason = reason;
        self.refresh_capabilities();
    }

    fn refresh_capabilities(&mut self) {
        use crate::data::automation::CapabilityEvidence;
        let mut sources = Vec::new();
        if self.native_full() {
            sources.push(ReadinessSource::Native);
        }
        if self.osc_usable() {
            sources.push(ReadinessSource::Osc7501);
        }
        let unavailable = self.native.facility == Some(false) || !self.config.native_channel;
        let osc_impossible = !self.config.osc_enabled || !self.config.delivery;
        let t = &mut self.snapshot.capabilities.terminal_turns;
        match self.snapshot.status_support {
            StatusSupport::Full | StatusSupport::BestEffort => {
                t.supported = Some(true);
                t.status_support = Some(self.snapshot.status_support);
                t.sources = sources;
                t.reason = self.snapshot.status_reason.clone();
                self.snapshot.capabilities.evidence = CapabilityEvidence::Observed;
            }
            StatusSupport::None if unavailable && osc_impossible => {
                t.supported = Some(false);
                t.status_support = Some(StatusSupport::None);
                t.sources = Vec::new();
                t.reason = self.snapshot.status_reason.clone();
            }
            StatusSupport::None => {
                t.supported = None;
                t.status_support = None;
                t.reason = self.snapshot.status_reason.clone();
            }
        }
    }

    fn ready_now(&self) -> bool {
        if self.turn.is_some()
            || self.stopping
            || self.ended
            || self.close_pending
            || self.update.is_some()
            || self.protocol_failed
            || self.native.veto.is_some()
        {
            return false;
        }
        match self.snapshot.status_support {
            StatusSupport::Full => self.native.ready,
            StatusSupport::BestEffort => {
                self.osc.idle
                    && !self.osc.descendant_veto
                    && self.osc.blocked.is_none()
                    && !self.osc.error
            }
            StatusSupport::None => false,
        }
    }

    fn known_blocked(&self) -> Option<BlockedReason> {
        match self.native.veto {
            Some(Veto::Blocked(reason)) => Some(reason),
            Some(_) => None,
            None if self.snapshot.status_support != StatusSupport::Full => self.osc.blocked,
            None => None,
        }
    }

    fn derive_state(&self) -> AutomationState {
        if self.ended {
            return AutomationState::Ended;
        }
        if self.stopping {
            return AutomationState::Stopping;
        }
        if let Some(update) = self.update {
            return update;
        }
        if let Some(reason) = self.known_blocked() {
            return match reason {
                BlockedReason::Auth => AutomationState::BlockedAuth,
                BlockedReason::Permission => AutomationState::BlockedPermission,
                BlockedReason::Input => AutomationState::BlockedInput,
            };
        }
        if let Some(turn) = &self.turn {
            return match turn.phase {
                TurnPhase::Acknowledged => AutomationState::Working,
                _ => AutomationState::Submitting,
            };
        }
        if self.ready_now() {
            return AutomationState::Ready;
        }
        if !self.startup_satisfied {
            AutomationState::Starting
        } else {
            AutomationState::Unknown
        }
    }

    /// Finish a step: derive state, report startup, and bump revision once
    /// if anything visible changed (or `force`).
    fn commit(&mut self, before: AutomationSnapshot, force: bool, effects: &mut Vec<Effect>) {
        self.retier();
        self.snapshot.state = self.derive_state();
        self.snapshot.active_turn_id = self.turn.as_ref().map(|t| t.turn_id.clone());
        if self.snapshot.state == AutomationState::Ready
            && !self.startup_satisfied
            && self.config.minimum.is_met_by(self.snapshot.status_support)
        {
            self.startup_satisfied = true;
            effects.push(Effect::StartupSatisfied);
        }
        if self.startup_satisfied && self.turn.is_none() {
            self.established = if self.established == StatusSupport::None {
                self.snapshot.status_support
            } else {
                self.established.weakest(self.snapshot.status_support)
            };
        }
        let mut compare = self.snapshot.clone();
        compare.revision = before.revision;
        if force || compare != before {
            self.snapshot.revision = before.revision + 1;
            effects.insert(0, Effect::State);
        }
    }

    // ── Observations ────────────────────────────────────────────────────

    /// Fold one Layer 1 observation in. Observations of another execution
    /// are ignored (stale producers cannot affect a replacement).
    pub fn observe(&mut self, observation: &Observation) -> Vec<Effect> {
        let mut effects = Vec::new();
        if observation.binding.execution_id != self.snapshot.execution_id || self.ended {
            return effects;
        }
        let before = self.snapshot.clone();
        // Fence stale producers: an older channel generation, or a
        // sequence at or before the last one applied from this source (a
        // replay), changes nothing.
        let binding = &observation.binding;
        let (generation, last) = match binding.source {
            ObservationSource::Native => {
                (&mut self.native.generation, &mut self.native.last_sequence)
            }
            ObservationSource::Osc7501 => (&mut self.osc.generation, &mut self.osc.last_sequence),
        };
        if binding.generation < *generation
            || (binding.generation == *generation && binding.sequence <= *last)
        {
            return effects;
        }
        *last = binding.sequence;
        if binding.source == ObservationSource::Native && binding.generation > *generation {
            // A fresh channel generation is not a ready signal.
            *generation = binding.generation;
            self.native.ready = false;
        }
        // Source establishment first: a tier change invalidates readiness
        // held *before* this record; the record itself lies after that
        // invalidation boundary and may establish readiness. Full native
        // support is established only by explicit facility evidence (see
        // `native_observation`), never implied by a readiness record.
        if binding.source == ObservationSource::Osc7501
            && self.config.osc_enabled
            && matches!(
                observation.kind,
                ObservationKind::OscIdle
                    | ObservationKind::OscWorking
                    | ObservationKind::OscDone
                    | ObservationKind::OscError
                    | ObservationKind::Blocked(_)
            )
        {
            self.osc.seen = true;
            self.retier();
        }
        match observation.binding.source {
            ObservationSource::Native => self.native_observation(observation, &mut effects),
            ObservationSource::Osc7501 => self.osc_observation(observation, &mut effects),
        }
        self.commit(before, false, &mut effects);
        effects
    }

    fn protocol_failure(&mut self, reason: &str, effects: &mut Vec<Effect>) {
        if self.protocol_failed {
            return;
        }
        self.protocol_failed = true;
        let f = failure(codes::AGENT_PROTOCOL_ERROR, reason);
        if let Some(turn) = self.turn.take() {
            effects.push(self.turn_result(
                turn,
                TurnStatus::Failed,
                CompletionSource::None,
                None,
                Some(f.clone()),
            ));
        }
        if !self.stopping {
            self.stopping = true;
            effects.push(Effect::Stop(f));
        }
    }

    /// The root identity is known and, for an exact resume, is the
    /// requested conversation. Native readiness requires it.
    fn identity_confirmed(&self) -> bool {
        match (&self.snapshot.native_session_id, &self.requested_native) {
            (None, _) => false,
            (Some(seen), Some(requested)) => seen == requested,
            (Some(_), None) => true,
        }
    }

    /// Whether the observation was received before the last readiness
    /// invalidation (it cannot establish readiness after it).
    fn before_watermark(&self, observation: &Observation) -> bool {
        observation.binding.sequence <= self.readiness_watermark
    }

    /// Take a new invalidation boundary on the host receive clock.
    fn invalidate_readiness(&mut self) {
        self.native.ready = false;
        self.osc.idle = false;
        self.readiness_watermark = crate::data::automation_observation::next_receive_sequence();
    }

    fn native_observation(&mut self, observation: &Observation, effects: &mut Vec<Effect>) {
        if !self.config.native_channel {
            return;
        }
        match &observation.kind {
            ObservationKind::Identity { native_session_id } => {
                if let Some(requested) = &self.requested_native {
                    if requested != native_session_id {
                        self.protocol_failed = true;
                        if !self.stopping {
                            self.stopping = true;
                            effects.push(Effect::Stop(failure(
                                codes::NATIVE_SESSION_NOT_FOUND,
                                "the provider did not resume the requested conversation",
                            )));
                        }
                        return;
                    }
                }
                match &self.snapshot.native_session_id {
                    None => {
                        self.snapshot.native_session_id = Some(native_session_id.clone());
                        effects.push(Effect::Identity(native_session_id.clone()));
                    }
                    Some(known) if known == native_session_id => {}
                    Some(_) => {
                        self.protocol_failure("contradictory native session identity", effects)
                    }
                }
            }
            ObservationKind::Facility {
                full_lifecycle,
                reason,
            } => {
                self.native.facility = *full_lifecycle;
                self.native.facility_reason = reason.clone();
                // New facility evidence is not readiness.
                self.native.ready = false;
                if *full_lifecycle == Some(false)
                    && self.config.minimum == MinimumStatusSupport::Full
                    && !self.startup_satisfied
                    && !self.stopping
                {
                    self.stopping = true;
                    effects.push(Effect::Stop(failure(
                        codes::STATUS_SUPPORT_INSUFFICIENT,
                        &format!(
                            "--automation=require_full: the native lifecycle facility is unavailable{}",
                            reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()
                        ),
                    )));
                }
            }
            ObservationKind::Ready => {
                // Explicit readiness of the confirmed root session under the
                // complete facility, after the last invalidation, with no
                // latched native activity. It never clears a veto: only a
                // correlated settlement or resynchronization does.
                if self.turn.is_none()
                    && self.native.healthy
                    && self.native.facility == Some(true)
                    && self.native.veto.is_none()
                    && self.identity_confirmed()
                    && !self.before_watermark(observation)
                {
                    self.native.ready = true;
                }
            }
            ObservationKind::PromptAccepted {
                native_turn_id,
                prompt_digest,
            } => {
                let correlated = self.turn.as_ref().is_some_and(|t| {
                    matches!(t.phase, TurnPhase::WriteStarted | TurnPhase::Written)
                        && observation.binding.sequence > t.delivered_after
                        && native_turn_id.is_some()
                        && prompt_digest.is_some()
                        && prompt_digest == &t.prompt_digest
                });
                // Accepted work (guarded or manual) latches a native veto
                // that survives source loss.
                self.native.veto = Some(Veto::Working);
                self.native.ready = false;
                if correlated {
                    let turn = self.turn.as_mut().expect("correlated turn");
                    turn.phase = TurnPhase::Acknowledged;
                    turn.native_turn_id = native_turn_id.clone();
                    effects.push(Effect::TurnStarted {
                        turn_id: turn.turn_id.clone(),
                        native_turn_id: native_turn_id.clone(),
                        source: StatusSource::Native,
                    });
                }
            }
            ObservationKind::Working => {
                // Real activity: latch (or replace a blocked) veto.
                self.native.veto = Some(Veto::Working);
                self.native.ready = false;
            }
            ObservationKind::Blocked(reason) => {
                self.native.veto = Some(Veto::Blocked(*reason));
                self.native.ready = false;
            }
            ObservationKind::Continuation => {
                self.native.veto = Some(Veto::Continuation);
                self.native.ready = false;
            }
            ObservationKind::Settled {
                native_turn_id,
                outcome,
            } => match self.turn.as_ref().map(|t| t.phase) {
                Some(TurnPhase::Acknowledged) => {
                    let matches = native_turn_id.is_some()
                        && self.turn.as_ref().and_then(|t| t.native_turn_id.as_ref())
                            == native_turn_id.as_ref();
                    if !matches {
                        // A settlement of other (manual) native work cannot
                        // complete the guarded turn; nor can one without an
                        // identity. A guarded turn acknowledged through OSC
                        // has no native turn ID to match.
                        if self
                            .turn
                            .as_ref()
                            .is_some_and(|t| t.native_turn_id.is_some())
                        {
                            self.protocol_failure(
                                "settled record for another native turn",
                                effects,
                            );
                        }
                        return;
                    }
                    let turn = self.turn.take().expect("acknowledged turn");
                    self.native.veto = None;
                    self.native.ready = false;
                    let result = match outcome {
                        NativeOutcome::Succeeded { final_message } => self.turn_result(
                            turn,
                            TurnStatus::Completed,
                            CompletionSource::Native,
                            final_message.clone(),
                            None,
                        ),
                        NativeOutcome::Failed { message } => self.turn_result(
                            turn,
                            TurnStatus::Failed,
                            CompletionSource::Native,
                            None,
                            Some(failure(codes::AGENT_TURN_FAILED, message)),
                        ),
                    };
                    effects.push(result);
                }
                Some(_) => self.protocol_failure("turn settled before acceptance", effects),
                None => {
                    self.native.veto = None;
                    self.native.ready = false;
                }
            },
            ObservationKind::SourceLost { reason } => {
                // Loss is never idle: any latched veto is retained.
                self.native.healthy = false;
                self.native.ready = false;
                self.native.lost = Some(reason.clone());
            }
            ObservationKind::Resynchronized { active, blocked } => {
                self.native.healthy = true;
                self.native.lost = None;
                self.native.ready = false;
                self.native.veto = match (blocked, active) {
                    (Some(reason), _) => Some(Veto::Blocked(*reason)),
                    (None, true) => Some(Veto::Working),
                    (None, false) => None,
                };
            }
            ObservationKind::ProtocolFailure { reason } => {
                let reason = reason.clone();
                self.protocol_failure(&reason, effects)
            }
            _ => {}
        }
    }

    fn osc_observation(&mut self, observation: &Observation, effects: &mut Vec<Effect>) {
        if !self.config.osc_enabled {
            return;
        }
        let generation = observation.binding.generation;
        if generation != self.osc.generation {
            // A new stream generation: nothing of the old one survives.
            self.osc.generation = generation;
            self.osc.idle = false;
            self.osc.working_after_delivery = false;
        }
        // OSC never overrules an authoritative native source.
        let authoritative = self.snapshot.status_support != StatusSupport::Full
            && self.tier_after_seen() == StatusSupport::BestEffort;
        match &observation.kind {
            ObservationKind::OscIdle => {
                self.osc.seen = true;
                // An idle report received before the last invalidation is
                // not fresh readiness.
                self.osc.idle = !self.before_watermark(observation);
                self.osc.blocked = None;
                self.osc.error = false;
            }
            ObservationKind::OscWorking => {
                self.osc.seen = true;
                self.osc.idle = false;
                self.osc.blocked = None;
                let inferable = self.tier_after_seen() == StatusSupport::BestEffort;
                let sequence = observation.binding.sequence;
                if let Some(turn) = &mut self.turn {
                    // Only a report received after the write was authorized
                    // can be about this delivery.
                    if inferable
                        && turn.phase >= TurnPhase::Written
                        && sequence > turn.delivered_after
                    {
                        self.osc.working_after_delivery = true;
                        if turn.phase == TurnPhase::Written {
                            turn.phase = TurnPhase::Acknowledged;
                            turn.delivered_osc_generation = generation;
                            effects.push(Effect::TurnStarted {
                                turn_id: turn.turn_id.clone(),
                                native_turn_id: None,
                                source: StatusSource::Osc7501,
                            });
                        }
                    }
                }
            }
            ObservationKind::OscDone => {
                self.osc.seen = true;
                self.osc.idle = false;
                // Settlement needs a clean lifecycle: no native activity,
                // no unresolved root blocked/error and no descendant veto.
                let settles = authoritative
                    && self.native.veto.is_none()
                    && self.osc.blocked.is_none()
                    && !self.osc.error
                    && !self.osc.descendant_veto
                    && self.osc.working_after_delivery
                    && self.turn.as_ref().is_some_and(|t| {
                        t.phase == TurnPhase::Acknowledged
                            && t.delivered_osc_generation == generation
                    });
                if settles {
                    let turn = self.turn.take().expect("settling turn");
                    self.osc.working_after_delivery = false;
                    effects.push(self.turn_result(
                        turn,
                        TurnStatus::CompletedUnverified,
                        CompletionSource::Osc7501,
                        None,
                        None,
                    ));
                }
            }
            ObservationKind::OscError => {
                self.osc.seen = true;
                self.osc.idle = false;
                self.osc.error = true;
                if authoritative && self.native.veto.is_none() {
                    if let Some(turn) = self.turn.take() {
                        self.osc.working_after_delivery = false;
                        effects.push(self.turn_result(
                            turn,
                            TurnStatus::Failed,
                            CompletionSource::Osc7501,
                            None,
                            Some(failure(
                                codes::AGENT_TURN_FAILED,
                                "the agent reported an error (OSC 7501)",
                            )),
                        ));
                    }
                }
            }
            ObservationKind::Blocked(reason) => {
                self.osc.seen = true;
                self.osc.idle = false;
                self.osc.blocked = Some(*reason);
            }
            ObservationKind::OscDescendantVeto(veto) => {
                self.osc.descendant_veto = *veto;
                if *veto {
                    self.osc.idle = false;
                }
            }
            ObservationKind::OscInvalidated { reason } => {
                // Unresolved root blocked/error evidence survives a reset:
                // only a later root idle (or working, for blocked) clears it.
                self.osc.idle = false;
                self.osc.working_after_delivery = false;
                if reason.contains("ended") {
                    self.osc.seen = false;
                }
            }
            _ => {}
        }
    }

    /// The tier once the current OSC report has established the source.
    fn tier_after_seen(&self) -> StatusSupport {
        if self.native_full() {
            StatusSupport::Full
        } else if self.config.osc_enabled && self.config.delivery {
            StatusSupport::BestEffort
        } else {
            StatusSupport::None
        }
    }

    fn turn_result(
        &mut self,
        turn: ActiveTurn,
        status: TurnStatus,
        completion_source: CompletionSource,
        final_message: Option<String>,
        failure: Option<AutomationFailure>,
    ) -> Effect {
        let verified = completion_source == CompletionSource::Native;
        Effect::TurnCompleted {
            turn_id: turn.turn_id,
            result: TurnResult {
                status,
                status_support: turn.weakest,
                readiness_source: turn.readiness_source,
                completion_source,
                completion_verified: verified,
                native_turn_id: turn.native_turn_id,
                final_message: if status == TurnStatus::Completed {
                    final_message
                } else {
                    None
                },
                output_status: OutputStatus::Complete,
                failure,
            },
        }
    }

    // ── Guarded operations ──────────────────────────────────────────────

    /// The checks for a *new* request, in the contract's order. Dedup and
    /// owner access precede this in the controller.
    pub fn check_operation(
        &self,
        execution: &ExecutionId,
        grant_epoch: Option<u64>,
        expected_revision: u64,
        close: bool,
    ) -> Result<(), Refusal> {
        if execution != &self.snapshot.execution_id || self.ended {
            return Err(Refusal::new(
                codes::STALE_EXECUTION,
                "the execution is not the terminal's current execution",
            ));
        }
        if grant_epoch.is_none() {
            return Err(Refusal::new(
                codes::INPUT_NOT_ALLOWED,
                "the connection does not hold the terminal's input grant",
            ));
        }
        if expected_revision != self.snapshot.revision {
            return Err(Refusal::new(
                codes::STALE_REVISION,
                format!(
                    "expected revision {expected_revision}, current revision {}",
                    self.snapshot.revision
                ),
            ));
        }
        if !close {
            if !self.config.minimum.is_met_by(self.snapshot.status_support) {
                return Err(Refusal::new(
                    codes::STATUS_SUPPORT_INSUFFICIENT,
                    format!(
                        "minimum status support {} is not met by current {}",
                        self.config.minimum.as_str(),
                        self.snapshot.status_support.as_str()
                    ),
                ));
            }
            if !self.config.delivery {
                return Err(Refusal::new(
                    codes::NATIVE_CAPABILITY_UNAVAILABLE,
                    "no established task delivery on this route",
                ));
            }
        }
        if self.snapshot.state != AutomationState::Ready || !self.ready_now() {
            return Err(Refusal::new(
                codes::AGENT_NOT_READY,
                format!("the agent is {}", self.snapshot.state.as_str()),
            ));
        }
        Ok(())
    }

    /// The accepted submission's snapshot change (not yet committed): the
    /// controller persists it atomically with the turn and dedup records.
    pub fn accept_submit(
        &mut self,
        turn_id: TurnId,
        prompt_digest: String,
        grant_epoch: Option<u64>,
    ) -> Vec<Effect> {
        let before = self.snapshot.clone();
        let (support, source) = self.tier();
        self.invalidate_readiness();
        self.osc.working_after_delivery = false;
        self.turn = Some(ActiveTurn {
            turn_id,
            phase: TurnPhase::Reserved,
            weakest: support,
            readiness_source: if source == StatusSource::Native {
                ReadinessSource::Native
            } else {
                ReadinessSource::Osc7501
            },
            native_turn_id: None,
            prompt_digest: Some(prompt_digest),
            grant_epoch,
            delivered_osc_generation: self.osc.generation,
            delivered_after: u64::MAX,
        });
        let mut effects = Vec::new();
        self.commit(before, true, &mut effects);
        effects
    }

    /// Undo an acceptance whose durable reservation failed (no bytes, no
    /// reply was published).
    pub fn abandon_reservation(&mut self, turn_id: &TurnId, before: AutomationSnapshot) {
        if self.turn.as_ref().is_some_and(|t| &t.turn_id == turn_id) {
            self.turn = None;
            self.snapshot = before;
        }
    }

    /// The writer's ingress callback, immediately before the write begins.
    /// On refusal the accepted turn fails with that code and zero bytes.
    pub fn writer_ingress(
        &mut self,
        turn_id: &TurnId,
        grant_epoch: Option<u64>,
    ) -> Result<Vec<Effect>, (Refusal, Vec<Effect>)> {
        let before = self.snapshot.clone();
        let refusal = match &self.turn {
            Some(t) if &t.turn_id == turn_id && t.phase == TurnPhase::Reserved => {
                if self.stopping || self.ended {
                    Some(Refusal::new(codes::CANCELLED, "the execution is stopping"))
                } else if self.update.is_some() {
                    Some(Refusal::new(
                        codes::AGENT_NOT_READY,
                        "a credential update began before delivery",
                    ))
                } else if !self.config.minimum.is_met_by(self.snapshot.status_support) {
                    Some(Refusal::new(
                        codes::STATUS_SUPPORT_INSUFFICIENT,
                        "the status source degraded below the launch minimum before delivery",
                    ))
                } else if self.snapshot.status_support == StatusSupport::None {
                    Some(Refusal::new(
                        codes::AGENT_NOT_READY,
                        "the status source was lost before delivery",
                    ))
                } else if t.grant_epoch.is_some() && grant_epoch != t.grant_epoch {
                    Some(Refusal::new(
                        codes::CONTROL_HELD,
                        "terminal control changed before delivery",
                    ))
                } else {
                    None
                }
            }
            _ => {
                return Err((
                    Refusal::new(codes::STALE_EXECUTION, "the turn is no longer pending"),
                    Vec::new(),
                ))
            }
        };
        let mut effects = Vec::new();
        match refusal {
            None => {
                let turn = self.turn.as_mut().expect("pending turn");
                turn.phase = TurnPhase::WriteStarted;
                turn.delivered_after = crate::data::automation_observation::next_receive_sequence();
                self.commit(before, false, &mut effects);
                Ok(effects)
            }
            Some(refusal) => {
                let turn = self.turn.take().expect("pending turn");
                let status = if refusal.code == codes::CANCELLED {
                    TurnStatus::Cancelled
                } else {
                    TurnStatus::Failed
                };
                effects.push(self.turn_result(
                    turn,
                    status,
                    CompletionSource::None,
                    None,
                    Some(refusal.failure()),
                ));
                self.commit(before, false, &mut effects);
                Err((refusal, effects))
            }
        }
    }

    /// The grant epoch the pending turn was accepted under.
    pub fn active_turn_grant(&self, turn_id: &TurnId) -> Option<u64> {
        self.turn
            .as_ref()
            .filter(|t| &t.turn_id == turn_id)
            .and_then(|t| t.grant_epoch)
    }

    /// Ingress was authorized but its durable `WriteStarted` marker failed:
    /// no byte is written. The turn fails and the execution stops (input is
    /// never authorized on uncommitted state).
    pub fn write_not_started(&mut self, turn_id: &TurnId, f: AutomationFailure) -> Vec<Effect> {
        let before = self.snapshot.clone();
        let mut effects = Vec::new();
        if self
            .turn
            .as_ref()
            .is_some_and(|t| &t.turn_id == turn_id && t.phase == TurnPhase::WriteStarted)
        {
            let turn = self.turn.take().expect("turn");
            effects.push(self.turn_result(
                turn,
                TurnStatus::Failed,
                CompletionSource::None,
                None,
                Some(f.clone()),
            ));
        }
        if !self.stopping {
            self.stopping = true;
            effects.push(Effect::Stop(f));
        }
        self.commit(before, false, &mut effects);
        effects
    }

    /// The write finished (`ok`) or failed after it may have started.
    pub fn write_finished(&mut self, turn_id: &TurnId, ok: bool) -> Vec<Effect> {
        let before = self.snapshot.clone();
        let mut effects = Vec::new();
        let Some(turn) = self.turn.as_mut().filter(|t| &t.turn_id == turn_id) else {
            return effects;
        };
        if ok {
            if turn.phase == TurnPhase::WriteStarted {
                turn.phase = TurnPhase::Written;
            }
        } else {
            let turn = self.turn.take().expect("turn");
            let f = failure(
                codes::DELIVERY_UNKNOWN,
                "the task write failed after it began; part of it may have reached the agent",
            );
            effects.push(self.turn_result(
                turn,
                TurnStatus::DeliveryUnknown,
                CompletionSource::None,
                None,
                Some(f.clone()),
            ));
            if !self.stopping {
                self.stopping = true;
                effects.push(Effect::Stop(f));
            }
        }
        self.commit(before, false, &mut effects);
        effects
    }

    /// Whether raw manual input is admitted now. Admitted input invalidates
    /// readiness before it is enqueued; it never becomes a turn.
    pub fn manual_input(&mut self) -> Result<Vec<Effect>, Refusal> {
        match self.snapshot.state {
            AutomationState::Ready
            | AutomationState::BlockedAuth
            | AutomationState::BlockedPermission
            | AutomationState::BlockedInput
            | AutomationState::Unknown => {
                let before = self.snapshot.clone();
                self.invalidate_readiness();
                let mut effects = Vec::new();
                self.commit(before, false, &mut effects);
                Ok(effects)
            }
            state => Err(Refusal::new(
                codes::INPUT_NOT_ALLOWED,
                format!(
                    "raw input is not accepted while the agent is {}",
                    state.as_str()
                ),
            )),
        }
    }

    /// The input grant changed holder (or was released).
    pub fn control_changed(&mut self, grant_epoch: Option<u64>) -> Vec<Effect> {
        let before = self.snapshot.clone();
        self.grant_epoch = grant_epoch;
        self.invalidate_readiness();
        let mut effects = Vec::new();
        self.commit(before, true, &mut effects);
        effects
    }

    /// An accepted idle close: `stopping`, delivery revoked.
    pub fn accept_close(&mut self) -> Vec<Effect> {
        let before = self.snapshot.clone();
        self.close_pending = true;
        self.stopping = true;
        let mut effects = Vec::new();
        self.commit(before, true, &mut effects);
        effects
    }

    /// Cancellation (or another stop): `stopping`; the outstanding turn
    /// ends cancelled, or `delivery_unknown` if bytes may be in flight.
    pub fn cancel(&mut self) -> Vec<Effect> {
        let before = self.snapshot.clone();
        let mut effects = Vec::new();
        self.stopping = true;
        if let Some(turn) = self.turn.take() {
            let (status, f) = match turn.phase {
                TurnPhase::WriteStarted | TurnPhase::Written => (
                    TurnStatus::DeliveryUnknown,
                    failure(
                        codes::DELIVERY_UNKNOWN,
                        "cancelled before the agent acknowledged the task",
                    ),
                ),
                _ => (
                    TurnStatus::Cancelled,
                    failure(codes::CANCELLED, "the command was cancelled"),
                ),
            };
            effects.push(self.turn_result(turn, status, CompletionSource::None, None, Some(f)));
        }
        self.commit(before, false, &mut effects);
        effects
    }

    /// The startup deadline passed. Fails only an unsatisfied startup.
    pub fn startup_expired(&mut self) -> Vec<Effect> {
        if self.startup_satisfied || self.stopping || self.ended {
            return Vec::new();
        }
        let before = self.snapshot.clone();
        self.stopping = true;
        let mut effects = vec![Effect::Stop(failure(
            codes::STARTUP_TIMEOUT,
            &format!(
                "the agent did not reach {} readiness before the startup deadline ({})",
                self.config.minimum.as_str(),
                self.tier_reason()
            ),
        ))];
        self.commit(before, false, &mut effects);
        effects
    }

    /// The acknowledgement deadline for `turn_id` passed.
    pub fn acknowledgement_expired(&mut self, turn_id: &TurnId) -> Vec<Effect> {
        let pending = self
            .turn
            .as_ref()
            .is_some_and(|t| &t.turn_id == turn_id && t.phase < TurnPhase::Acknowledged);
        if !pending {
            return Vec::new();
        }
        let before = self.snapshot.clone();
        let turn = self.turn.take().expect("pending turn");
        let bytes_possible = turn.phase >= TurnPhase::WriteStarted;
        let f = if bytes_possible {
            failure(
                codes::DELIVERY_UNKNOWN,
                "the agent did not acknowledge the task before the acknowledgement deadline",
            )
        } else {
            failure(codes::AGENT_NOT_READY, "the task was never delivered")
        };
        let status = if bytes_possible {
            TurnStatus::DeliveryUnknown
        } else {
            TurnStatus::Failed
        };
        let mut effects =
            vec![self.turn_result(turn, status, CompletionSource::None, None, Some(f.clone()))];
        if !self.stopping {
            self.stopping = true;
            effects.push(Effect::Stop(f));
        }
        self.commit(before, false, &mut effects);
        effects
    }

    /// The provider process exited (or the controller finalizes). Any
    /// outstanding turn ends; never replayed.
    pub fn process_ended(&mut self) -> Vec<Effect> {
        let before = self.snapshot.clone();
        let mut effects = Vec::new();
        if let Some(turn) = self.turn.take() {
            let (status, f) = match turn.phase {
                TurnPhase::Reserved => (
                    TurnStatus::Failed,
                    failure(codes::AGENT_TURN_FAILED, "the agent exited before delivery"),
                ),
                TurnPhase::WriteStarted | TurnPhase::Written => (
                    TurnStatus::DeliveryUnknown,
                    failure(
                        codes::DELIVERY_UNKNOWN,
                        "the agent exited before acknowledging the task",
                    ),
                ),
                TurnPhase::Acknowledged => (
                    TurnStatus::Failed,
                    failure(
                        codes::AGENT_TURN_FAILED,
                        "the agent exited before the turn settled",
                    ),
                ),
            };
            effects.push(self.turn_result(turn, status, CompletionSource::None, None, Some(f)));
        }
        self.stopping = true;
        self.commit(before, false, &mut effects);
        effects
    }

    /// Final transition: `ended`, all input refused.
    pub fn end(&mut self) -> Vec<Effect> {
        let before = self.snapshot.clone();
        let mut effects = Vec::new();
        if self.turn.is_some() {
            effects.extend(self.process_ended());
        }
        self.ended = true;
        self.commit(before, false, &mut effects);
        effects
    }

    pub fn credential_observed(&mut self) -> Vec<Effect> {
        let before = self.snapshot.clone();
        let mut effects = Vec::new();
        self.commit(before, true, &mut effects);
        effects
    }

    // ── Credential-update seam (B8) ─────────────────────────────────────

    /// Reserve `credential_updating` or `restarting`. The underlying
    /// active-turn/blocked state is retained in the cell.
    pub fn begin_update(&mut self, restarting: bool) -> Result<Vec<Effect>, Refusal> {
        if self.update.is_some() || self.stopping || self.ended || self.close_pending {
            return Err(Refusal::new(
                codes::AGENT_NOT_READY,
                "another lifecycle operation is pending",
            ));
        }
        let before = self.snapshot.clone();
        self.update = Some(if restarting {
            AutomationState::Restarting
        } else {
            AutomationState::CredentialUpdating
        });
        self.invalidate_readiness();
        let mut effects = Vec::new();
        self.commit(before, true, &mut effects);
        Ok(effects)
    }

    /// End the update; readiness requires fresh evidence afterwards.
    pub fn end_update(&mut self) -> Vec<Effect> {
        let before = self.snapshot.clone();
        self.update = None;
        self.invalidate_readiness();
        let mut effects = Vec::new();
        self.commit(before, true, &mut effects);
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::agent_policy::NativeStartMode;
    use crate::data::automation::{AutomationMode, CapabilityEvidence};
    use crate::data::automation_observation::ObservationBinding;
    use crate::data::automation_records::*;
    use crate::data::workspace::WorkspaceAccess;

    fn snapshot(mode: AutomationMode) -> AutomationSnapshot {
        AutomationSnapshot {
            execution_id: ExecutionId::new(),
            provider: "claude".into(),
            provider_version: None,
            native_session_id: None,
            automation_mode: mode,
            minimum_status_support: mode.minimum_status_support(),
            state: AutomationState::Starting,
            revision: 0,
            active_turn_id: None,
            native_start_mode: NativeStartMode::Default,
            workspace_access: WorkspaceAccess::ReadWrite,
            status_support: StatusSupport::None,
            status_source: StatusSource::None,
            status_reason: None,
            capabilities: AutomationCapabilities {
                agent: "claude".into(),
                runtime: "docker".into(),
                provider_version: None,
                image_digest: None,
                minimum_version: None,
                version_warning: None,
                evidence: CapabilityEvidence::Declaration,
                native_plan: NativePlanCapability {
                    interactive: None,
                    headless: None,
                    acp: None,
                    kind: None,
                },
                headless_turns: SupportCapability {
                    supported: None,
                    reason: None,
                },
                terminal_turns: TerminalTurnsCapability {
                    supported: None,
                    status_support: None,
                    sources: vec![],
                    reason: None,
                },
                native_resume: NativeResumeCapability {
                    interactive: None,
                    headless: None,
                    reason: None,
                },
                settings_profiles: true,
                credential_updates: Default::default(),
            },
        }
    }

    fn cell(mode: AutomationMode, native: bool, osc: bool) -> AdmissionCell {
        AdmissionCell::new(
            snapshot(mode),
            AdmissionConfig {
                minimum: mode.minimum_status_support(),
                osc_enabled: osc,
                native_channel: native,
                delivery: true,
            },
            None,
        )
    }

    struct Feed {
        osc_generation: u64,
    }
    impl Feed {
        fn new() -> Self {
            Self { osc_generation: 1 }
        }
        /// The complete native contract: confirmed identity, explicit full
        /// facility evidence, then explicit readiness.
        fn establish(&mut self, c: &mut AdmissionCell) -> Vec<Effect> {
            self.native(
                c,
                ObservationKind::Identity {
                    native_session_id: NativeSessionId::parse("s1").unwrap(),
                },
            );
            self.native(
                c,
                ObservationKind::Facility {
                    full_lifecycle: Some(true),
                    reason: None,
                },
            );
            self.native(c, ObservationKind::Ready)
        }
        fn native(&mut self, c: &mut AdmissionCell, kind: ObservationKind) -> Vec<Effect> {
            c.observe(&Observation {
                binding: ObservationBinding {
                    execution_id: c.execution_id().clone(),
                    source: ObservationSource::Native,
                    generation: 1,
                    sequence: crate::data::automation_observation::next_receive_sequence(),
                },
                kind,
            })
        }
        fn osc(&mut self, c: &mut AdmissionCell, kind: ObservationKind) -> Vec<Effect> {
            c.observe(&Observation {
                binding: ObservationBinding {
                    execution_id: c.execution_id().clone(),
                    source: ObservationSource::Osc7501,
                    generation: self.osc_generation,
                    sequence: crate::data::automation_observation::next_receive_sequence(),
                },
                kind,
            })
        }
    }

    fn submit(c: &mut AdmissionCell) -> TurnId {
        let id = c.execution_id().clone();
        c.check_operation(&id, Some(1), c.revision(), false)
            .expect("admissible");
        let turn = TurnId::new();
        c.accept_submit(turn.clone(), "digest".into(), Some(1));
        assert!(c.writer_ingress(&turn, Some(1)).is_ok());
        c.write_finished(&turn, true);
        turn
    }

    fn completed(effects: &[Effect]) -> Option<&TurnResult> {
        effects.iter().find_map(|e| match e {
            Effect::TurnCompleted { result, .. } => Some(result),
            _ => None,
        })
    }

    #[test]
    fn starts_starting_at_revision_zero_and_osc_idle_makes_best_effort_ready() {
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        assert_eq!(c.snapshot().state, AutomationState::Starting);
        assert_eq!(c.revision(), 0);
        let e = f.osc(&mut c, ObservationKind::OscIdle);
        assert!(e.contains(&Effect::StartupSatisfied));
        assert_eq!(c.snapshot().state, AutomationState::Ready);
        assert_eq!(c.snapshot().status_support, StatusSupport::BestEffort);
        assert_eq!(c.snapshot().status_source, StatusSource::Osc7501);
        assert_eq!(c.revision(), 1);
        // An identical repeat does not churn revision.
        assert!(f.osc(&mut c, ObservationKind::OscIdle).is_empty());
        assert_eq!(c.revision(), 1);
    }

    #[test]
    fn osc_working_then_done_completes_unverified_and_done_is_not_ready() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        let turn = submit(&mut c);
        assert_eq!(c.snapshot().state, AutomationState::Submitting);
        // Idle while a turn is pending does not complete it.
        assert!(completed(&f.osc(&mut c, ObservationKind::OscIdle)).is_none());
        let e = f.osc(&mut c, ObservationKind::OscWorking);
        assert!(e.iter().any(|e| matches!(e, Effect::TurnStarted { turn_id, source: StatusSource::Osc7501, .. } if *turn_id == turn)));
        assert_eq!(c.snapshot().state, AutomationState::Working);
        let e = f.osc(&mut c, ObservationKind::OscDone);
        let r = completed(&e).expect("settled");
        assert_eq!(r.status, TurnStatus::CompletedUnverified);
        assert!(!r.completion_verified);
        assert_eq!(r.final_message, None);
        assert_eq!(r.completion_source, CompletionSource::Osc7501);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_eq!(c.snapshot().state, AutomationState::Ready);
    }

    #[test]
    fn done_without_working_never_completes() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        submit(&mut c);
        assert!(completed(&f.osc(&mut c, ObservationKind::OscDone)).is_none());
        assert_eq!(c.snapshot().state, AutomationState::Submitting);
    }

    #[test]
    fn native_full_contract_two_turns_with_verified_results() {
        let mut c = cell(AutomationMode::RequireFull, true, true);
        let mut f = Feed::new();
        let e = f.establish(&mut c);
        assert!(e.contains(&Effect::StartupSatisfied));
        assert_eq!(c.snapshot().status_support, StatusSupport::Full);
        for answer in ["first", ""] {
            let turn = submit(&mut c);
            f.native(
                &mut c,
                ObservationKind::PromptAccepted {
                    native_turn_id: Some("n1".into()),
                    prompt_digest: Some("digest".into()),
                },
            );
            assert_eq!(c.snapshot().state, AutomationState::Working);
            // Continuation keeps the turn active and settles nothing.
            f.native(&mut c, ObservationKind::Continuation);
            assert_eq!(c.snapshot().active_turn_id.as_ref(), Some(&turn));
            let e = f.native(
                &mut c,
                ObservationKind::Settled {
                    native_turn_id: Some("n1".into()),
                    outcome: NativeOutcome::Succeeded {
                        final_message: Some(answer.into()),
                    },
                },
            );
            let r = completed(&e).unwrap();
            assert_eq!(r.status, TurnStatus::Completed);
            assert!(r.completion_verified);
            assert_eq!(r.final_message.as_deref(), Some(answer));
            assert_ne!(c.snapshot().state, AutomationState::Ready);
            f.native(&mut c, ObservationKind::Ready);
        }
    }

    #[test]
    fn require_full_refuses_best_effort_without_writes() {
        let mut c = cell(AutomationMode::RequireFull, true, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_eq!(c.snapshot().status_support, StatusSupport::BestEffort);
        assert!(!c.startup_satisfied());
        let id = c.execution_id().clone();
        let r = c
            .check_operation(&id, Some(1), c.revision(), false)
            .unwrap_err();
        assert_eq!(r.code, codes::STATUS_SUPPORT_INSUFFICIENT);
        // Confirmed absence fails a strict launch promptly.
        let e = f.native(
            &mut c,
            ObservationKind::Facility {
                full_lifecycle: Some(false),
                reason: Some("no input-ready hook".into()),
            },
        );
        assert!(e
            .iter()
            .any(|e| matches!(e, Effect::Stop(f) if f.code == codes::STATUS_SUPPORT_INSUFFICIENT)));
    }

    #[test]
    fn native_loss_falls_back_to_osc_with_fresh_idle_and_restoration_needs_resync() {
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        f.establish(&mut c);
        assert_eq!(c.snapshot().status_support, StatusSupport::Full);
        let rev = c.revision();
        f.native(
            &mut c,
            ObservationKind::SourceLost {
                reason: "gap".into(),
            },
        );
        assert_eq!(c.snapshot().status_support, StatusSupport::BestEffort);
        assert!(c.revision() > rev);
        // The cached OSC idle was discarded at the tier change.
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_eq!(c.snapshot().state, AutomationState::Ready);
        // One Ready hook cannot restore full without resynchronization.
        f.native(&mut c, ObservationKind::Ready);
        assert_eq!(c.snapshot().status_support, StatusSupport::BestEffort);
        f.native(
            &mut c,
            ObservationKind::Resynchronized {
                active: false,
                blocked: None,
            },
        );
        assert_eq!(c.snapshot().status_support, StatusSupport::Full);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        f.native(&mut c, ObservationKind::Ready);
        assert_eq!(c.snapshot().state, AutomationState::Ready);
    }

    #[test]
    fn native_veto_survives_loss_and_blocks_osc_readiness() {
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        f.establish(&mut c);
        f.native(&mut c, ObservationKind::Blocked(BlockedReason::Permission));
        assert_eq!(c.snapshot().state, AutomationState::BlockedPermission);
        f.native(
            &mut c,
            ObservationKind::SourceLost {
                reason: "eof".into(),
            },
        );
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_eq!(c.snapshot().state, AutomationState::BlockedPermission);
    }

    #[test]
    fn source_loss_before_write_fails_the_turn_with_zero_bytes() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        let id = c.execution_id().clone();
        c.check_operation(&id, Some(1), c.revision(), false)
            .unwrap();
        let turn = TurnId::new();
        c.accept_submit(turn.clone(), "d".into(), Some(1));
        f.osc(
            &mut c,
            ObservationKind::OscInvalidated {
                reason: "terminal stream ended".into(),
            },
        );
        let (refusal, effects) = c.writer_ingress(&turn, Some(1)).unwrap_err();
        // The source left the tier below the floor before any byte.
        assert_eq!(refusal.code, codes::STATUS_SUPPORT_INSUFFICIENT);
        assert_eq!(completed(&effects).unwrap().status, TurnStatus::Failed);
    }

    #[test]
    fn control_change_before_write_and_stale_revision_are_refused() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        let id = c.execution_id().clone();
        assert_eq!(
            c.check_operation(&id, Some(1), c.revision() + 1, false)
                .unwrap_err()
                .code,
            codes::STALE_REVISION
        );
        assert_eq!(
            c.check_operation(&id, None, c.revision(), false)
                .unwrap_err()
                .code,
            codes::INPUT_NOT_ALLOWED
        );
        assert_eq!(
            c.check_operation(&ExecutionId::new(), Some(1), c.revision(), false)
                .unwrap_err()
                .code,
            codes::STALE_EXECUTION
        );
        let turn = TurnId::new();
        c.accept_submit(turn.clone(), "d".into(), Some(1));
        let (refusal, _) = c.writer_ingress(&turn, Some(2)).unwrap_err();
        assert_eq!(refusal.code, codes::CONTROL_HELD);
    }

    #[test]
    fn manual_input_gate_follows_the_state_table() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        assert!(c.manual_input().is_err(), "starting refuses");
        f.osc(&mut c, ObservationKind::OscIdle);
        assert!(c.manual_input().is_ok());
        assert_eq!(c.snapshot().state, AutomationState::Unknown);
        f.osc(&mut c, ObservationKind::OscIdle);
        submit(&mut c);
        assert!(c.manual_input().is_err(), "submitting refuses");
        f.osc(&mut c, ObservationKind::OscWorking);
        assert!(c.manual_input().is_err(), "working refuses");
        f.osc(&mut c, ObservationKind::Blocked(BlockedReason::Auth));
        assert_eq!(c.snapshot().state, AutomationState::BlockedAuth);
        assert!(c.manual_input().is_ok(), "blocked allows");
        // Answering the prompt does not complete the turn.
        assert!(c.snapshot().active_turn_id.is_some());
    }

    #[test]
    fn startup_and_acknowledgement_deadlines() {
        let mut c = cell(AutomationMode::Auto, true, true);
        let e = c.startup_expired();
        assert!(e
            .iter()
            .any(|e| matches!(e, Effect::Stop(f) if f.code == codes::STARTUP_TIMEOUT)));
        assert_eq!(c.snapshot().state, AutomationState::Stopping);

        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        assert!(
            c.startup_expired().is_empty(),
            "satisfied startup never expires"
        );
        let turn = submit(&mut c);
        let e = c.acknowledgement_expired(&turn);
        assert_eq!(completed(&e).unwrap().status, TurnStatus::DeliveryUnknown);
        assert!(e.iter().any(|e| matches!(e, Effect::Stop(_))));
    }

    #[test]
    fn cancel_and_close() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        let id = c.execution_id().clone();
        c.check_operation(&id, Some(1), c.revision(), true).unwrap();
        c.accept_close();
        assert_eq!(c.snapshot().state, AutomationState::Stopping);
        assert_eq!(
            c.check_operation(&id, Some(1), c.revision(), true)
                .unwrap_err()
                .code,
            codes::AGENT_NOT_READY
        );

        let mut c = cell(AutomationMode::Auto, false, true);
        f.osc(&mut c, ObservationKind::OscIdle);
        submit(&mut c);
        let e = c.cancel();
        assert_eq!(completed(&e).unwrap().status, TurnStatus::DeliveryUnknown);
        c.end();
        assert_eq!(c.snapshot().state, AutomationState::Ended);
    }

    #[test]
    fn protocol_errors_are_terminal_and_not_hidden_by_fallback() {
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        f.establish(&mut c);
        submit(&mut c);
        let e = f.native(
            &mut c,
            ObservationKind::Settled {
                native_turn_id: None,
                outcome: NativeOutcome::Succeeded {
                    final_message: None,
                },
            },
        );
        let r = completed(&e).unwrap();
        assert_eq!(r.status, TurnStatus::Failed);
        assert_eq!(
            r.failure.as_ref().unwrap().code,
            codes::AGENT_PROTOCOL_ERROR
        );
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
    }

    #[test]
    fn descendant_veto_and_reset_invalidate_osc_readiness() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscDescendantVeto(true));
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        f.osc(&mut c, ObservationKind::OscDescendantVeto(false));
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_eq!(c.snapshot().state, AutomationState::Ready);
        f.osc(
            &mut c,
            ObservationKind::OscInvalidated {
                reason: "terminal reset".into(),
            },
        );
        assert_ne!(c.snapshot().state, AutomationState::Ready);
    }

    #[test]
    fn a_stale_osc_generation_cannot_complete_a_turn() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        submit(&mut c);
        f.osc(&mut c, ObservationKind::OscWorking);
        f.osc_generation += 1;
        assert!(completed(&f.osc(&mut c, ObservationKind::OscDone)).is_none());
    }

    #[test]
    fn disabled_osc_and_no_native_stays_unknown_and_unavailable() {
        let mut c = cell(AutomationMode::Auto, false, false);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_eq!(c.snapshot().status_support, StatusSupport::None);
        assert_eq!(c.snapshot().state, AutomationState::Starting);
        assert_eq!(
            c.snapshot().capabilities.terminal_turns.supported,
            Some(false)
        );
    }

    #[test]
    fn requested_resume_identity_mismatch_stops() {
        let mut c = AdmissionCell::new(
            snapshot(AutomationMode::Auto),
            AdmissionConfig {
                minimum: MinimumStatusSupport::BestEffort,
                osc_enabled: true,
                native_channel: true,
                delivery: true,
            },
            Some(NativeSessionId::parse("wanted").unwrap()),
        );
        let mut f = Feed::new();
        let e = f.native(
            &mut c,
            ObservationKind::Identity {
                native_session_id: NativeSessionId::parse("other").unwrap(),
            },
        );
        assert!(e
            .iter()
            .any(|e| matches!(e, Effect::Stop(f) if f.code == codes::NATIVE_SESSION_NOT_FOUND)));
    }

    #[test]
    fn weakest_tier_drops_on_loss_during_a_turn() {
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        f.establish(&mut c);
        submit(&mut c);
        f.native(
            &mut c,
            ObservationKind::PromptAccepted {
                native_turn_id: Some("n1".into()),
                prompt_digest: Some("digest".into()),
            },
        );
        f.native(
            &mut c,
            ObservationKind::SourceLost {
                reason: "eof".into(),
            },
        );
        let e = c.cancel();
        let r = completed(&e).unwrap();
        assert_eq!(r.status, TurnStatus::Cancelled);
        assert_eq!(r.status_support, StatusSupport::BestEffort);
        assert_eq!(r.readiness_source, ReadinessSource::Native);
    }

    // ── WI 0070 review regressions (automation findings 4–7) ───────────

    #[test]
    fn identity_free_or_facility_free_native_records_never_establish_full() {
        // Ready with no identity and no facility evidence: nothing.
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        f.native(&mut c, ObservationKind::Ready);
        assert_eq!(c.snapshot().status_support, StatusSupport::None);
        assert_eq!(c.snapshot().state, AutomationState::Starting);
        // Facility without identity: full tier, but never ready.
        f.native(
            &mut c,
            ObservationKind::Facility {
                full_lifecycle: Some(true),
                reason: None,
            },
        );
        f.native(&mut c, ObservationKind::Ready);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        // An exact resume must confirm the requested conversation first.
        let mut c = AdmissionCell::new(
            snapshot(AutomationMode::Auto),
            AdmissionConfig {
                minimum: MinimumStatusSupport::BestEffort,
                osc_enabled: false,
                native_channel: true,
                delivery: true,
            },
            Some(NativeSessionId::parse("s1").unwrap()),
        );
        f.native(
            &mut c,
            ObservationKind::Facility {
                full_lifecycle: Some(true),
                reason: None,
            },
        );
        f.native(&mut c, ObservationKind::Ready);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        f.establish(&mut c);
        assert_eq!(c.snapshot().state, AutomationState::Ready);
    }

    #[test]
    fn acceptance_and_settlement_need_matching_native_identities() {
        let mut c = cell(AutomationMode::RequireFull, true, true);
        let mut f = Feed::new();
        f.establish(&mut c);
        submit(&mut c);
        // An acceptance missing its turn identity or digest is uncorrelated.
        f.native(
            &mut c,
            ObservationKind::PromptAccepted {
                native_turn_id: None,
                prompt_digest: Some("digest".into()),
            },
        );
        assert_eq!(c.snapshot().state, AutomationState::Submitting);
        f.native(
            &mut c,
            ObservationKind::PromptAccepted {
                native_turn_id: Some("n1".into()),
                prompt_digest: Some("digest".into()),
            },
        );
        assert_eq!(c.snapshot().state, AutomationState::Working);
        // A settlement without (or with another) turn identity never
        // verifies this turn.
        let e = f.native(
            &mut c,
            ObservationKind::Settled {
                native_turn_id: None,
                outcome: NativeOutcome::Succeeded {
                    final_message: Some("x".into()),
                },
            },
        );
        let r = completed(&e).expect("protocol failure ends the turn");
        assert_eq!(r.status, TurnStatus::Failed);
        assert!(!r.completion_verified);
        assert_eq!(
            r.failure.as_ref().unwrap().code,
            codes::AGENT_PROTOCOL_ERROR
        );
    }

    #[test]
    fn native_work_latches_a_veto_that_osc_cannot_settle_after_loss() {
        // Working while ready leaves the execution not ready.
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        f.establish(&mut c);
        assert_eq!(c.snapshot().state, AutomationState::Ready);
        f.native(&mut c, ObservationKind::Working);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        // A later Ready does not clear it; only a settlement does.
        f.native(&mut c, ObservationKind::Ready);
        assert_ne!(c.snapshot().state, AutomationState::Ready);

        // An acknowledged native turn survives source loss: OSC done
        // cannot complete it and later idle admits nothing.
        let mut c = cell(AutomationMode::Auto, true, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        f.establish(&mut c);
        submit(&mut c);
        f.native(
            &mut c,
            ObservationKind::PromptAccepted {
                native_turn_id: Some("n1".into()),
                prompt_digest: Some("digest".into()),
            },
        );
        f.native(
            &mut c,
            ObservationKind::SourceLost {
                reason: "eof".into(),
            },
        );
        assert_eq!(c.snapshot().status_support, StatusSupport::BestEffort);
        f.osc(&mut c, ObservationKind::OscWorking);
        assert!(completed(&f.osc(&mut c, ObservationKind::OscDone)).is_none());
        assert!(c.snapshot().active_turn_id.is_some());
        f.osc(&mut c, ObservationKind::OscIdle);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
    }

    #[test]
    fn osc_done_respects_unresolved_root_and_descendant_vetoes() {
        for veto in [
            ObservationKind::Blocked(BlockedReason::Permission),
            ObservationKind::OscError,
            ObservationKind::OscDescendantVeto(true),
        ] {
            let mut c = cell(AutomationMode::Auto, false, true);
            let mut f = Feed::new();
            f.osc(&mut c, ObservationKind::OscIdle);
            submit(&mut c);
            f.osc(&mut c, ObservationKind::OscWorking);
            let error = veto == ObservationKind::OscError;
            let e = f.osc(&mut c, veto);
            if error {
                // A root error fails the turn; it is never a success.
                assert_eq!(completed(&e).unwrap().status, TurnStatus::Failed);
                continue;
            }
            let done = f.osc(&mut c, ObservationKind::OscDone);
            assert!(completed(&done).is_none(), "done settled despite a veto");
            assert!(c.snapshot().active_turn_id.is_some());
        }
        // Root blocked evidence survives a terminal reset.
        let mut c = cell(AutomationMode::Auto, false, true);
        let mut f = Feed::new();
        f.osc(&mut c, ObservationKind::OscIdle);
        submit(&mut c);
        f.osc(&mut c, ObservationKind::OscWorking);
        f.osc(&mut c, ObservationKind::Blocked(BlockedReason::Input));
        f.osc(
            &mut c,
            ObservationKind::OscInvalidated {
                reason: "terminal reset".into(),
            },
        );
        assert_eq!(c.snapshot().state, AutomationState::BlockedInput);
    }

    #[test]
    fn replayed_stale_and_pre_invalidation_observations_cannot_restore_readiness() {
        let mut c = cell(AutomationMode::Auto, false, true);
        let id = c.execution_id().clone();
        let stamp = |kind, generation| Observation {
            binding: ObservationBinding {
                execution_id: id.clone(),
                source: ObservationSource::Osc7501,
                generation,
                sequence: crate::data::automation_observation::next_receive_sequence(),
            },
            kind,
        };
        // Received (stamped) before the control change, applied after.
        let early = stamp(ObservationKind::OscIdle, 1);
        let first = stamp(ObservationKind::OscIdle, 1);
        c.observe(&first);
        assert_eq!(c.snapshot().state, AutomationState::Ready);
        c.control_changed(Some(2));
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        c.observe(&early);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        // A literal replay of the applied record changes nothing.
        c.observe(&first);
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        // A newer generation, then an older one: the older is fenced.
        c.observe(&stamp(ObservationKind::OscIdle, 3));
        assert_eq!(c.snapshot().state, AutomationState::Ready);
        c.manual_input().unwrap();
        c.observe(&stamp(ObservationKind::OscIdle, 2));
        assert_ne!(c.snapshot().state, AutomationState::Ready);
        // A fresh report after the boundary is readiness again.
        c.observe(&stamp(ObservationKind::OscIdle, 3));
        assert_eq!(c.snapshot().state, AutomationState::Ready);
    }
}
