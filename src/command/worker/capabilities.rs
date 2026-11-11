//! Facts shared by status projection and work-server registration.
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::data::config::daemon::{Interaction, WorkerSettings};
use crate::engine::worker::lease_workspace::LeaseRuntimeSupport;

pub(super) fn features(
    support: LeaseRuntimeSupport,
    interaction: Interaction,
    allow_override: bool,
    terminal_hub: bool,
    credential_helper: bool,
    inline_source: bool,
) -> Vec<&'static str> {
    use awman_wsp::features::*;
    let mut result = support.features();
    let catalogue = CommandCatalogue::get();
    for (path, names) in [
        (&["exec", "prompt"][..], &[PROMPT][..]),
        (
            &["exec", "workflow"][..],
            &[WORKFLOW_DYNAMIC, WORKFLOW_FILE][..],
        ),
    ] {
        if catalogue.lookup(path).is_some_and(|spec| spec.api_allowed) {
            result.extend_from_slice(names);
        }
    }
    let interactive = !interaction.is_headless() || allow_override;
    if interactive {
        result.push(INTERACTIVE);
        if terminal_hub {
            result.push(TERMINAL);
        }
    }
    result.extend([OUTPUTS_MOUNT_STATE, WORKFLOW_STATE_EVENTS]);
    if credential_helper {
        result.extend([LEASE_CREDENTIALS, LEASE_GIT_AUTH_FAILURE, LEASE_PUSH_RETRY]);
    }
    if inline_source {
        result.push(LEASE_INLINE_WORK_ITEM);
    }
    result
}

pub(super) fn for_settings(
    support: LeaseRuntimeSupport,
    settings: &WorkerSettings,
) -> Vec<&'static str> {
    features(
        support,
        settings.pull_interaction,
        settings.pull_allow_override,
        true,
        true,
        true,
    )
}
