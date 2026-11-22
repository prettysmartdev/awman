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
