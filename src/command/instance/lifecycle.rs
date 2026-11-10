//! The hosted instance's frontend lifecycle (WI 0142 Part A §2), as a pure
//! state machine. It maps frontend connections and user actions onto today's
//! TUI lifecycle and nothing else: there is no "detached instance" state.
//!
//! | Event | Outcome |
//! |---|---|
//! | `Quit` from any frontend (after its `QuitConfirm`) | today's TUI-exit policy; every frontend gets `Detach{quit}` |
//! | primary disconnects without `Quit` | 2 s grace for a reconnect, then today's killed-TUI policy (`PrimaryLost`) |
//! | secondary disconnects | nothing |
//! | last tab closes | the instance quits, as today |
//!
//! Time is passed in (`now`), so the grace period is tested without sleeping.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::data::frontend_protocol::{DetachReasonWire, PRIMARY_RECONNECT_GRACE};
use crate::data::ids::FrontendId;

/// How long a reserved instance (`POST /v1/engine/instances`) waits for its
/// primary `Hello` before it is discarded.
pub const PRIMARY_HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an instance quit. Every reason runs today's TUI exit policy
/// ([`super::EngineInstance::quit`]); only the frontends' `Detach` differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReason {
    /// A frontend sent `Quit` (it already showed `QuitConfirm`).
    UserQuit,
    /// The primary frontend went away and did not reconnect in the grace.
    PrimaryLost,
    /// The last tab closed.
    LastTabClosed,
    /// `DELETE /v1/engine/instances/{id}`.
    Api,
    /// The `engine` feature was disabled (drain or stop).
    EngineDisabled,
    /// The daemon is stopping.
    DaemonShutdown,
    /// The primary `Hello` never arrived after the instance was reserved.
    NeverAttached,
}

impl QuitReason {
    /// The `Detach` every remaining frontend receives.
    pub fn detach_reason(self) -> DetachReasonWire {
        match self {
            Self::UserQuit | Self::LastTabClosed | Self::Api => DetachReasonWire::Quit,
            Self::PrimaryLost | Self::NeverAttached => DetachReasonWire::InstanceEnded,
            Self::EngineDisabled => DetachReasonWire::EngineDisabled,
            Self::DaemonShutdown => DetachReasonWire::DaemonShutdown,
        }
    }
}

/// What the host must do after a lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Nothing,
    /// The primary went away: keep the instance until `deadline`.
    Grace {
        deadline: Instant,
    },
    /// End the instance with today's exit policy.
    Quit(QuitReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Primary {
    /// Reserved; the primary `Hello` has not arrived yet.
    Awaiting {
        deadline: Instant,
    },
    Connected(FrontendId),
    Reconnecting {
        id: FrontendId,
        deadline: Instant,
    },
}

/// Why a frontend may not attach (see `admission`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleRefusal {
    PrimaryExists,
    /// A secondary arrived before the primary started the instance.
    NotStarted,
    Ended,
}

/// One hosted instance's frontend lifecycle.
#[derive(Debug, Clone)]
pub struct FrontendLifecycle {
    primary: Primary,
    secondaries: BTreeSet<FrontendId>,
    ended: Option<QuitReason>,
    grace: Duration,
}

impl FrontendLifecycle {
    /// A reserved instance created at `now`.
    pub fn reserved(now: Instant) -> Self {
        Self {
            primary: Primary::Awaiting {
                deadline: now + PRIMARY_HELLO_TIMEOUT,
            },
            secondaries: BTreeSet::new(),
            ended: None,
            grace: PRIMARY_RECONNECT_GRACE,
        }
    }

    pub fn set_reconnect_grace(&mut self, grace: Duration) {
        self.grace = grace;
    }

    pub fn ended(&self) -> Option<QuitReason> {
        self.ended
    }

    /// Whether the primary has attached at least once.
    pub fn started(&self) -> bool {
        !matches!(self.primary, Primary::Awaiting { .. })
    }

    /// The connected primary, if any.
    pub fn primary(&self) -> Option<&FrontendId> {
        match &self.primary {
            Primary::Connected(id) => Some(id),
            _ => None,
        }
    }

    /// The primary waiting for its reconnect, if any.
    pub fn reconnecting(&self) -> Option<&FrontendId> {
        match &self.primary {
            Primary::Reconnecting { id, .. } => Some(id),
            _ => None,
        }
    }

    pub fn connected_frontends(&self) -> usize {
        self.secondaries.len() + usize::from(self.primary().is_some())
    }

    /// A first primary `Hello` (`resume: None`). Returns whether this
    /// started the instance (the first tab must be opened).
    pub fn attach_primary(&mut self, id: FrontendId) -> Result<bool, LifecycleRefusal> {
        if self.ended.is_some() {
            return Err(LifecycleRefusal::Ended);
        }
        match &self.primary {
            Primary::Awaiting { .. } => {
                self.primary = Primary::Connected(id);
                Ok(true)
            }
            // A second primary, even while the first is reconnecting: only a
            // `resume` with the first's id may take its place.
            Primary::Connected(_) | Primary::Reconnecting { .. } => {
                Err(LifecycleRefusal::PrimaryExists)
            }
        }
    }

    /// A primary `Hello { resume: Some(id) }`: the same frontend back within
    /// its grace. Anything else is a second primary.
    pub fn resume_primary(
        &mut self,
        id: &FrontendId,
        now: Instant,
    ) -> Result<(), LifecycleRefusal> {
        if self.ended.is_some() {
            return Err(LifecycleRefusal::Ended);
        }
        match &self.primary {
            Primary::Reconnecting {
                id: waiting,
                deadline,
            } if waiting == id && now < *deadline => {
                self.primary = Primary::Connected(id.clone());
                Ok(())
            }
            Primary::Awaiting { .. } => Err(LifecycleRefusal::NotStarted),
            _ => Err(LifecycleRefusal::PrimaryExists),
        }
    }

    pub fn attach_secondary(&mut self, id: FrontendId) -> Result<(), LifecycleRefusal> {
        if self.ended.is_some() {
            return Err(LifecycleRefusal::Ended);
        }
        if !self.started() {
            return Err(LifecycleRefusal::NotStarted);
        }
        self.secondaries.insert(id);
        Ok(())
    }

    /// A frontend's connection closed without `Quit`.
    pub fn disconnected(&mut self, id: &FrontendId, now: Instant) -> LifecycleAction {
        if self.ended.is_some() {
            return LifecycleAction::Nothing;
        }
        if self.secondaries.remove(id) {
            return LifecycleAction::Nothing;
        }
        match &self.primary {
            Primary::Connected(primary) if primary == id => {
                let deadline = now + self.grace;
                self.primary = Primary::Reconnecting {
                    id: id.clone(),
                    deadline,
                };
                LifecycleAction::Grace { deadline }
            }
            _ => LifecycleAction::Nothing,
        }
    }

    /// Advance time: an expired grace is today's killed-TUI policy; an
    /// expired reservation discards the instance.
    pub fn poll(&mut self, now: Instant) -> LifecycleAction {
        if self.ended.is_some() {
            return LifecycleAction::Nothing;
        }
        match &self.primary {
            Primary::Reconnecting { deadline, .. } if now >= *deadline => {
                self.end(QuitReason::PrimaryLost)
            }
            Primary::Awaiting { deadline } if now >= *deadline => {
                self.end(QuitReason::NeverAttached)
            }
            _ => LifecycleAction::Nothing,
        }
    }

    /// A quit for `reason`. Idempotent: only the first quit acts.
    pub fn quit(&mut self, reason: QuitReason) -> LifecycleAction {
        if self.ended.is_some() {
            return LifecycleAction::Nothing;
        }
        self.end(reason)
    }

    /// A tab closed and `remaining` are left.
    pub fn tab_closed(&mut self, remaining: usize) -> LifecycleAction {
        if remaining == 0 {
            self.quit(QuitReason::LastTabClosed)
        } else {
            LifecycleAction::Nothing
        }
    }

    fn end(&mut self, reason: QuitReason) -> LifecycleAction {
        self.ended = Some(reason);
        LifecycleAction::Quit(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> (FrontendLifecycle, FrontendId, Instant) {
        let now = Instant::now();
        let mut lifecycle = FrontendLifecycle::reserved(now);
        let primary = FrontendId::new();
        assert_eq!(lifecycle.attach_primary(primary.clone()), Ok(true));
        (lifecycle, primary, now)
    }

    #[test]
    fn quit_ends_the_instance_once_with_a_quit_detach() {
        let (mut lifecycle, _, _) = started();
        assert_eq!(
            lifecycle.quit(QuitReason::UserQuit),
            LifecycleAction::Quit(QuitReason::UserQuit)
        );
        assert_eq!(
            lifecycle.quit(QuitReason::UserQuit),
            LifecycleAction::Nothing
        );
        assert_eq!(QuitReason::UserQuit.detach_reason(), DetachReasonWire::Quit);
        assert_eq!(
            lifecycle.attach_secondary(FrontendId::new()),
            Err(LifecycleRefusal::Ended)
        );
    }

    #[test]
    fn primary_loss_quits_only_after_the_grace() {
        let (mut lifecycle, primary, now) = started();
        let LifecycleAction::Grace { deadline } = lifecycle.disconnected(&primary, now) else {
            panic!("primary loss must start the grace");
        };
        assert_eq!(deadline, now + PRIMARY_RECONNECT_GRACE);
        assert_eq!(lifecycle.reconnecting(), Some(&primary));
        assert_eq!(
            lifecycle.poll(now + PRIMARY_RECONNECT_GRACE - Duration::from_millis(1)),
            LifecycleAction::Nothing
        );
        assert_eq!(
            lifecycle.poll(deadline),
            LifecycleAction::Quit(QuitReason::PrimaryLost)
        );
        assert_eq!(lifecycle.ended(), Some(QuitReason::PrimaryLost));
    }

    #[test]
    fn a_reconnect_within_the_grace_keeps_the_instance() {
        let (mut lifecycle, primary, now) = started();
        lifecycle.disconnected(&primary, now);
        let back = now + Duration::from_millis(1500);
        assert_eq!(lifecycle.resume_primary(&primary, back), Ok(()));
        assert_eq!(lifecycle.primary(), Some(&primary));
        assert_eq!(
            lifecycle.poll(now + Duration::from_secs(10)),
            LifecycleAction::Nothing
        );
    }

    #[test]
    fn a_late_or_foreign_resume_is_refused() {
        let (mut lifecycle, primary, now) = started();
        lifecycle.disconnected(&primary, now);
        assert_eq!(
            lifecycle.resume_primary(&FrontendId::new(), now),
            Err(LifecycleRefusal::PrimaryExists)
        );
        assert_eq!(
            lifecycle.resume_primary(&primary, now + PRIMARY_RECONNECT_GRACE),
            Err(LifecycleRefusal::PrimaryExists)
        );
    }

    #[test]
    fn secondary_loss_changes_nothing() {
        let (mut lifecycle, _, now) = started();
        let secondary = FrontendId::new();
        lifecycle.attach_secondary(secondary.clone()).unwrap();
        assert_eq!(lifecycle.connected_frontends(), 2);
        assert_eq!(
            lifecycle.disconnected(&secondary, now),
            LifecycleAction::Nothing
        );
        assert_eq!(lifecycle.connected_frontends(), 1);
        assert_eq!(lifecycle.ended(), None);
    }

    #[test]
    fn a_second_primary_is_refused_even_during_the_grace() {
        let (mut lifecycle, primary, now) = started();
        assert_eq!(
            lifecycle.attach_primary(FrontendId::new()),
            Err(LifecycleRefusal::PrimaryExists)
        );
        lifecycle.disconnected(&primary, now);
        assert_eq!(
            lifecycle.attach_primary(FrontendId::new()),
            Err(LifecycleRefusal::PrimaryExists)
        );
    }

    #[test]
    fn closing_the_last_tab_quits() {
        let (mut lifecycle, _, _) = started();
        assert_eq!(lifecycle.tab_closed(1), LifecycleAction::Nothing);
        assert_eq!(
            lifecycle.tab_closed(0),
            LifecycleAction::Quit(QuitReason::LastTabClosed)
        );
        assert_eq!(
            QuitReason::LastTabClosed.detach_reason(),
            DetachReasonWire::Quit
        );
    }

    #[test]
    fn a_reservation_without_a_primary_expires() {
        let now = Instant::now();
        let mut lifecycle = FrontendLifecycle::reserved(now);
        assert_eq!(
            lifecycle.attach_secondary(FrontendId::new()),
            Err(LifecycleRefusal::NotStarted)
        );
        assert_eq!(
            lifecycle.poll(now + PRIMARY_HELLO_TIMEOUT),
            LifecycleAction::Quit(QuitReason::NeverAttached)
        );
    }
}
