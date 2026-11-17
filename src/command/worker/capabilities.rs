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
    let mut result = features(
        support,
        settings.pull_interaction,
        settings.pull_allow_override,
        true,
        true,
        true,
    );
    if support.secret_env {
        result.push(awman_wsp::features::LEASE_SECRETS);
    }
    if support.skill_mounts {
        result.push(awman_wsp::features::LEASE_SKILLS);
    }
    // Both ride on `exec workflow`; advertise them only where it runs.
    if CommandCatalogue::get()
        .lookup(&["exec", "workflow"])
        .is_some_and(|spec| spec.api_allowed)
    {
        result.extend([
            awman_wsp::features::LEASE_PREREQUISITES,
            awman_wsp::features::LEASE_WORKFLOWS_DYNAMIC,
        ]);
    }
    if super::lease_context::SUPPORTED {
        result.push(awman_wsp::features::LEASE_CONTEXT);
    }
    if super::lease_workflow_context::SUPPORTED {
        result.push(awman_wsp::features::LEASE_WORKFLOW_CONTEXT);
    }
    result
}

#[cfg(test)]
mod lease_exec_tests {
    use super::*;
    use awman_wsp::features::{LEASE_SECRETS, LEASE_SKILLS};

    #[test]
    fn daemon_advertises_secrets_and_only_mount_capable_runtimes_advertise_skills() {
        let settings =
            crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
        for skill_mounts in [false, true] {
            let support = LeaseRuntimeSupport {
                read_only_workspace: false,
                outputs_mount: false,
                skill_mounts,
                secret_env: true,
            };
            let registered = for_settings(support, &settings);
            assert_eq!(
                registered
                    .iter()
                    .filter(|feature| **feature == LEASE_SECRETS)
                    .count(),
                1
            );
            assert_eq!(registered.contains(&LEASE_SKILLS), skill_mounts);
            let status = crate::command::daemon_api::capabilities::advertised(false, &registered);
            assert!(status.iter().any(|feature| feature == LEASE_SECRETS));
            assert_eq!(
                status.iter().any(|feature| feature == LEASE_SKILLS),
                skill_mounts
            );
            let foreground = features(support, settings.pull_interaction, false, true, true, true);
            assert!(!foreground.contains(&LEASE_SECRETS));
            assert!(!foreground.contains(&LEASE_SKILLS));
        }
        let sandbox = LeaseRuntimeSupport {
            read_only_workspace: false,
            outputs_mount: false,
            skill_mounts: false,
            secret_env: false,
        };
        let registered = for_settings(sandbox, &settings);
        assert!(!registered.contains(&LEASE_SECRETS));
        assert!(!registered.contains(&LEASE_SKILLS));
        let status = crate::command::daemon_api::capabilities::advertised(false, &registered);
        assert!(!status
            .iter()
            .any(|feature| feature == LEASE_SECRETS || feature == LEASE_SKILLS));
        let status = crate::command::daemon_api::capabilities::advertised(false, &[]);
        assert!(!status
            .iter()
            .any(|feature| feature == LEASE_SECRETS || feature == LEASE_SKILLS));
    }
}

#[cfg(test)]
mod lease_prerequisite_and_dynamic_capability_tests {
    use super::*;
    use awman_wsp::features::{LEASE_PREREQUISITES, LEASE_WORKFLOWS_DYNAMIC};

    #[test]
    fn daemon_advertises_both_on_every_runtime_and_the_foreground_host_neither() {
        let settings =
            crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
        for (skill_mounts, secret_env) in [(true, true), (false, false)] {
            let support = LeaseRuntimeSupport {
                read_only_workspace: false,
                outputs_mount: false,
                skill_mounts,
                secret_env,
            };
            let registered = for_settings(support, &settings);
            for feature in [LEASE_PREREQUISITES, LEASE_WORKFLOWS_DYNAMIC] {
                assert_eq!(registered.iter().filter(|f| **f == feature).count(), 1);
                let status =
                    crate::command::daemon_api::capabilities::advertised(false, &registered);
                assert!(status.iter().any(|c| c == feature));
                // Never without a pull worker reporting it.
                let status = crate::command::daemon_api::capabilities::advertised(false, &[]);
                assert!(!status.iter().any(|c| c == feature));
            }
            let foreground = features(support, settings.pull_interaction, false, true, true, true);
            assert!(!foreground.contains(&LEASE_PREREQUISITES));
            assert!(!foreground.contains(&LEASE_WORKFLOWS_DYNAMIC));
        }
        // Push advertises the prerequisite contract unconditionally.
        let status = crate::command::daemon_api::capabilities::advertised(false, &[]);
        assert_eq!(
            status
                .iter()
                .filter(|c| *c == "workflow_prerequisites")
                .count(),
            1
        );
    }
}
