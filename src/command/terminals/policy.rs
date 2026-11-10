//! Who may see and who may drive a terminal.

use std::collections::HashMap;

use crate::data::daemon_state::{CommandListener, FeatureName};
use crate::data::ids::TerminalId;
use crate::data::terminal::{ControlHolder, TerminalOwner, TerminalRecord, ViewerIdentity};

use super::TerminalRefusal;

/// The daemon settings admission depends on, snapshotted per call so a live
/// `config set` applies to the next viewer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessPolicy {
    /// The features the network listener serves: listed in
    /// `daemon.network.features` *and* enabled by config (persistently). An
    /// engine tab's terminals are local only unless `engine` is here.
    pub network_features: std::collections::BTreeMap<String, Vec<FeatureName>>,
    /// `daemon.terminal.remoteInput`: one half of the network-input gate.
    pub remote_input: bool,
}

impl AccessPolicy {
    /// May this viewer open this terminal at all? A command's terminals are
    /// served wherever `worker` is served, network listener included; an
    /// engine tab's only locally unless `engine` is network-listed; a
    /// lease's are relayed by 0143, not served here. Squad runs (`run_`)
    /// follow `squad`'s network listing the same way.
    pub fn admit(
        &self,
        record: &TerminalRecord,
        viewer: &ViewerIdentity,
    ) -> Result<(), TerminalRefusal> {
        if viewer.listener == CommandListener::Local {
            return Ok(());
        }
        let served = match &record.owner_id {
            TerminalOwner::Command(_) => self.network_serves(viewer, FeatureName::Worker),
            TerminalOwner::Tab(_) => self.network_serves(viewer, FeatureName::Engine),
            TerminalOwner::Run(_) => self.network_serves(viewer, FeatureName::Squad),
            TerminalOwner::Lease(_) => false,
        };
        if served {
            Ok(())
        } else {
            Err(TerminalRefusal::NetworkRefused)
        }
    }

    /// May this viewer hold the input grant? Local viewers: yes unless
    /// `read_only`. Network viewers: only when both `remote_input` and the
    /// record's `allow_remote_input` are set, and the client names itself
    /// with `x-awman-actor` (the grant is logged under that name).
    pub fn input_allowed(&self, record: &TerminalRecord, viewer: &ViewerIdentity) -> bool {
        if viewer.read_only {
            return false;
        }
        match viewer.listener {
            CommandListener::Local => true,
            // A browser viewer on a browser listener is an instance frontend,
            // not a remote network viewer: `remoteInput` does not gate it (D13).
            CommandListener::Network
                if matches!(
                    viewer.principal,
                    crate::data::daemon_state::Principal::Browser { .. }
                ) =>
            {
                true
            }
            CommandListener::Network => {
                self.remote_input
                    && record.allow_remote_input
                    && viewer.actor.as_deref().is_some_and(|a| !a.is_empty())
            }
        }
    }

    fn network_serves(&self, viewer: &ViewerIdentity, feature: FeatureName) -> bool {
        viewer
            .listener_name
            .as_ref()
            .and_then(|name| self.network_features.get(name))
            .is_some_and(|features| features.contains(&feature))
    }
}

/// Who holds the input grant per terminal, by name. Exclusivity itself is
/// the hub's [`InputGrant`](crate::engine::terminal_hub::InputGrant)
/// capability (two local viewers share the name `local` but never the
/// capability); this book answers "who holds it" for events and records.
#[derive(Debug, Default)]
pub struct InputGrants {
    holders: HashMap<TerminalId, ControlHolder>,
}

impl InputGrants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `holder` as holding the grant. `Err(ControlHeld)` when
    /// someone else is recorded.
    pub fn take(&mut self, id: &TerminalId, holder: ControlHolder) -> Result<(), TerminalRefusal> {
        match self.holders.get(id) {
            Some(current) if *current != holder => Err(TerminalRefusal::ControlHeld),
            _ => {
                self.holders.insert(id.clone(), holder);
                Ok(())
            }
        }
    }

    /// Release the grant if `holder` has it. Returns whether it was held.
    pub fn release(&mut self, id: &TerminalId, holder: &ControlHolder) -> bool {
        if self.holds(id, holder) {
            self.holders.remove(id);
            true
        } else {
            false
        }
    }

    pub fn holder(&self, id: &TerminalId) -> Option<&ControlHolder> {
        self.holders.get(id)
    }

    pub fn holds(&self, id: &TerminalId, holder: &ControlHolder) -> bool {
        self.holders.get(id) == Some(holder)
    }

    /// Drop whatever name is recorded for `id`. Used when the hub just
    /// handed out a fresh capability: a name left by a grant the PTY's exit
    /// revoked must not block the new holder.
    pub(super) fn clear(&mut self, id: &TerminalId) {
        self.holders.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ids::{CommandId, LeaseId, RunId, TabId};
    use crate::data::terminal::{Annotations, TerminalState};

    fn record(owner: TerminalOwner, allow_remote_input: bool) -> TerminalRecord {
        TerminalRecord {
            id: TerminalId::new(),
            owner_id: owner,
            feature: FeatureName::Worker,
            source: None,
            agent: "claude".into(),
            model: None,
            attempt: 1,
            cols: 120,
            rows: 40,
            state: TerminalState::Open,
            exit_code: None,
            created_at: chrono::Utc::now(),
            allow_remote_input,
        }
    }

    fn network(actor: Option<&str>) -> ViewerIdentity {
        ViewerIdentity {
            principal: crate::data::daemon_state::Principal::Owner,
            listener_name: Some("default".into()),
            listener: CommandListener::Network,
            actor: actor.map(str::to_owned),
            annotations: Annotations::new(),
            read_only: false,
        }
    }

    #[test]
    fn local_viewers_are_admitted_to_every_owner() {
        let policy = AccessPolicy::default();
        for owner in [
            TerminalOwner::Command(CommandId::new()),
            TerminalOwner::Tab(TabId::new()),
            TerminalOwner::Run(RunId::new()),
            TerminalOwner::Lease(LeaseId::new()),
        ] {
            assert!(policy
                .admit(&record(owner, false), &ViewerIdentity::local())
                .is_ok());
        }
    }

    /// D13: a browser viewer is an instance frontend; `remoteInput` and the
    /// record's `allow_remote_input` do not gate it, `read_only` still does.
    #[test]
    fn browser_viewers_hold_input_without_remote_input() {
        let policy = AccessPolicy {
            network_features: [("loopback".into(), vec![FeatureName::Engine])].into(),
            remote_input: false,
        };
        let tab = record(TerminalOwner::Tab(TabId::new()), false);
        let mut browser = network(Some("brw_00000000000000000000000000000000"));
        browser.listener_name = Some("loopback".into());
        browser.principal = crate::data::daemon_state::Principal::Browser {
            grant_id: "brw_00000000000000000000000000000000".into(),
            origin: "https://awman.prettysmart.dev".into(),
            listener: "loopback".into(),
        };
        assert!(policy.admit(&tab, &browser).is_ok());
        assert!(policy.input_allowed(&tab, &browser));
        browser.read_only = true;
        assert!(!policy.input_allowed(&tab, &browser));
        // An API-key viewer on the same listener stays gated.
        let mut key = network(Some("ci"));
        key.listener_name = Some("loopback".into());
        assert!(!policy.input_allowed(&tab, &key));
    }

    #[test]
    fn network_admission_follows_the_owner_feature() {
        let worker_only = AccessPolicy {
            network_features: [("default".into(), vec![FeatureName::Worker])].into(),
            remote_input: false,
        };
        let cmd = record(TerminalOwner::Command(CommandId::new()), false);
        let tab = record(TerminalOwner::Tab(TabId::new()), false);
        let lease = record(TerminalOwner::Lease(LeaseId::new()), false);
        assert!(worker_only.admit(&cmd, &network(None)).is_ok());
        assert_eq!(
            worker_only.admit(&tab, &network(None)),
            Err(TerminalRefusal::NetworkRefused)
        );
        assert_eq!(
            AccessPolicy::default().admit(&cmd, &network(None)),
            Err(TerminalRefusal::NetworkRefused)
        );
        let with_engine = AccessPolicy {
            network_features: [(
                "default".into(),
                vec![FeatureName::Worker, FeatureName::Engine],
            )]
            .into(),
            remote_input: false,
        };
        assert!(with_engine.admit(&tab, &network(None)).is_ok());
        assert_eq!(
            with_engine.admit(&lease, &network(None)),
            Err(TerminalRefusal::NetworkRefused)
        );
    }

    #[test]
    fn network_input_needs_both_settings_and_an_actor() {
        let on = AccessPolicy {
            network_features: [("default".into(), vec![FeatureName::Worker])].into(),
            remote_input: true,
        };
        let off = AccessPolicy {
            remote_input: false,
            ..on.clone()
        };
        let owner = || TerminalOwner::Command(CommandId::new());
        let allowed = record(owner(), true);
        let denied = record(owner(), false);
        assert!(on.input_allowed(&allowed, &network(Some("webapp"))));
        assert!(!on.input_allowed(&denied, &network(Some("webapp"))));
        assert!(!off.input_allowed(&allowed, &network(Some("webapp"))));
        assert!(!on.input_allowed(&allowed, &network(None)));
        assert!(!on.input_allowed(&allowed, &network(Some(""))));
        let mut read_only = ViewerIdentity::local();
        read_only.read_only = true;
        assert!(!on.input_allowed(&allowed, &read_only));
        assert!(off.input_allowed(&denied, &ViewerIdentity::local()));
    }

    #[test]
    fn input_grants_are_exclusive_by_name() {
        let id = TerminalId::new();
        let mut grants = InputGrants::new();
        grants.take(&id, ControlHolder::Actor("a".into())).unwrap();
        assert_eq!(
            grants.take(&id, ControlHolder::Actor("b".into())),
            Err(TerminalRefusal::ControlHeld)
        );
        assert!(!grants.release(&id, &ControlHolder::Actor("b".into())));
        assert!(grants.release(&id, &ControlHolder::Actor("a".into())));
        assert!(grants.holder(&id).is_none());
        grants.take(&id, ControlHolder::Local).unwrap();
        assert!(grants.holds(&id, &ControlHolder::Local));
        grants.clear(&id);
        assert!(grants.holder(&id).is_none());
    }
}
