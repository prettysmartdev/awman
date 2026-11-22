pub const CAPABILITIES: &[&str] = &[
    crate::data::settings_profile::SETTINGS_PROFILES_CAPABILITY,
    crate::data::settings_profile::NATIVE_SESSION_RESUME_CAPABILITY,
    // WI 0070 automation: owner-local capability declarations and turn
    // records, guarded terminal turns, typed automation results and the
    // four lifecycle event features. Route support is refined per agent by
    // runtime evidence, never implied by these names.
    crate::data::automation::AGENT_CAPABILITIES_CAPABILITY,
    crate::data::automation::AGENT_AUTOMATION_CAPABILITY,
    crate::data::automation::TERMINAL_TURNS_CAPABILITY,
    crate::data::automation::AUTOMATION_RESULTS_CAPABILITY,
    crate::data::automation::AGENT_TURN_RECORDS_CAPABILITY,
    crate::data::automation::AGENT_STATE_EVENT,
    crate::data::automation::AGENT_IDENTITY_EVENT,
    crate::data::automation::AGENT_TURN_STARTED_EVENT,
    crate::data::automation::AGENT_TURN_COMPLETED_EVENT,
    "context_api",
    "workflow_context",
    "daemon_features",
    "network_listener",
    "named_listeners",
    "api_keys",
    "health",
    "trace_propagation",
    "error_codes",
    "workflow_state",
    "command_cancel",
    "labels",
    "idempotency",
    "runs",
    // WI 0137: `result.{outcome,failure,failed_steps,git,pull_request,
    // final_message,outputs,data}` and the one final `command_status`.
    "structured_results",
    // WI 0137: `GET /v1/worker/commands/{id}/outputs[/{path}]`.
    "outputs",
    // WI 0137: `source` on the event envelope and `?step=` on the logs.
    "event_source",
    // WI 0137: `step_activity` events from structured-output agents.
    "step_activity",
    // WI 0138: `GET /v1/worker/commands/{id}`, the command record.
    "command_record",
    // WI 0138: `setup_status`/`setup_error` on session records and the
    // `?setup_status=` filter on `GET /v1/worker/sessions`.
    "session_setup_status",
    // WI 0138: `session_id` in the `POST /v1/worker/commands` body.
    "command_session_body",
    "remote_dynamic",
    "workdir_roots",
    "event_ids",
    "resource_routes",
    "feature_states",
    "config_routes",
    "grant_routes",
    "typed_ids",
    "list_pages",
    "headless_interaction",
    // WI 0141: `program_status` events (`{state, kind, progress, msg, app}`)
    // on `/v1/worker/commands/{id}/events` from agents that speak OSC 7501.
    "agent_status",
    // Engine slots and host processes now register real hub terminals.
    "terminal",
    "interactive",
    "engine_instances",
    "frontend_protocol",
    // WI 0143: worker push accepts `exec workflow --dynamic`, a workflow
    // file path and `exec prompt` (each proven by the api_parity suites).
    "workflow_dynamic",
    "workflow_file",
    "prompt",
    // WI 0143: a `workflow_state` event after every persisted snapshot.
    "workflow_state_events",
    // WI 0143: `outputs_mounted` on every command record that launched.
    "outputs_mount_state",
    // WI 0143: instance-owned queue requests over awman.frontend.v1.
    "worker_queue",
    // WI 0159: `/v1/browser-links*` pairing and `/v1/browsers*` grant
    // management (browser listeners / owner socket).
    "browser_links",
    // WI 0159: `/v1/browser-tickets` and ticketed WebSocket upgrades.
    "browser_tickets",
    "launch_contexts",
    "engine_catalogue",
    "web_frontend",
    // WI 0145 slice 1: uniform push-session Git authentication failures.
    "git_auth_failure",
    // WI 0145 slice 2: typed `push_retry` events (attempt, operation,
    // `delay_ms`, sanitized remote) while push-back retries.
    "push_retry_events",
    // WI 0145 slice 2: push-back results distinguish `push_unreachable`
    // (retries exhausted) and `cancelled` from `push_failed`.
    "push_retry_codes",
    // WI 0145 slice 3: `git_auth` on session creation, delivered to every
    // push-session Git operation through the one-shot socket helper
    // (`credentials_lost` after a restart), over Unix sockets or Windows pipes.
    "session_git_auth",
    // WI 0145: `secrets` on command submission, injected for `env(NAME)`
    // overlays, redacted from events/results, `secrets_lost` after restart.
    "command_secrets",
    // WI 0147: `work_item: { title, body, id? }` on `POST /v1/worker/commands`
    // (and `/v1/worker/runs`), staged privately as a remote work item.
    "inline_work_item",
    // WI 0147: `--work-item` accepts github / http providers / local files by
    // `workItems.source`; `--issue` was replaced.
    "work_item_sources",
    // WI 0148: `init` is available to API clients and returns its project
    // file list in the typed command result.
    "api_init",
    // WI 0163: `ready` is available to API clients with typed build results
    // and progress events.
    "api_ready",
    // WI 0149/0162: `exec workflow` accepts the four `--prerequisite-*` flags
    // (identity, ssh, token, unresolved; a value granting beyond the server's
    // global policy is refused `prerequisite_widening_refused`), emits a
    // `workflow_prerequisites` report event, and an unresolved prerequisite
    // fails with `failure.stage` `workflow_prerequisites`.
    "workflow_prerequisites",
    // WI 0152: `--read-only-workspace` on `exec prompt` / `exec workflow` and
    // per-step `read_only_workspace`. Advertised only by
    // [`advertised_for_runtime`] on a runtime that enforces it, never by
    // [`advertised`]; the WSP lease flag stays `lease_read_only`.
    crate::data::workspace::READ_ONLY_WORKSPACE_CAPABILITY,
];

/// WI 0143: capabilities of the pull worker, in their fixed order. Each is
/// advertised only while the daemon runs a pull worker that reports it
/// (`PullHandle::capabilities`), never speculatively.
pub const PULL_CAPABILITIES: &[&str] = &[
    "lease_image_reuse",
    "lease_secrets",
    "lease_skills",
    "lease_prerequisites",
    "lease_workflows_dynamic",
    "lease_context",
    "lease_workflow_context",
    "worker_pull",
    "lease_credentials",
    "lease_inline_work_item",
    "lease_read_only",
    "lease_git_auth_failure",
    "lease_push_retry",
];

/// The `capabilities` list of `/v1/status` without the runtime-dependent
/// `read_only_workspace`: every other static capability (`outputs` only when
/// the runtime mounts `/awman/output`), then the pull worker's, in
/// [`PULL_CAPABILITIES`] order. Prefer [`advertised_for_runtime`] wherever
/// the runtime's capabilities are reachable.
pub fn advertised(mounts_outputs: bool, pull: &[&str]) -> Vec<String> {
    advertised_with(mounts_outputs, false, pull)
}

/// [`advertised`], plus `read_only_workspace` when the runtime enforces a
/// read-only workspace mount (WI 0152). On a runtime that cannot (sbx) the
/// capability is absent, which is how a client learns that
/// `--read-only-workspace` would be refused.
pub fn advertised_for_runtime(
    caps: &crate::engine::agent_runtime::Capabilities,
    pull: &[&str],
) -> Vec<String> {
    let mut result = advertised_with(caps.mounts_outputs(true), caps.read_only_workspace(), pull);
    // Session setup rebuilds stale images only on runtimes that build images
    // locally. Imported builtin images do not use the image-input freshness
    // path, so do not advertise the capability there.
    if caps.image_acquisition == crate::engine::agent_runtime::ImageAcquisition::Build {
        result.push("image_inputs".to_string());
    }
    result
}

fn advertised_with(mounts_outputs: bool, read_only_workspace: bool, pull: &[&str]) -> Vec<String> {
    CAPABILITIES
        .iter()
        .filter(|c| enabled(c))
        .filter(|c| mounts_outputs || **c != "outputs")
        .filter(|c| {
            read_only_workspace || **c != crate::data::workspace::READ_ONLY_WORKSPACE_CAPABILITY
        })
        .filter(|c| crate::engine::git::credential_socket::supported() || **c != "session_git_auth")
        .chain(
            PULL_CAPABILITIES
                .iter()
                .filter(|c| pull.contains(c) && enabled(c)),
        )
        .map(|c| c.to_string())
        .collect()
}

fn enabled(capability: &str) -> bool {
    match capability {
        "context_api" => crate::command::context::repo_api::SUPPORTED,
        "workflow_context" => crate::command::context::workflow_api::SUPPORTED,
        "lease_context" => crate::command::worker::lease_context::SUPPORTED,
        "lease_workflow_context" => crate::command::worker::lease_workflow_context::SUPPORTED,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_capability_is_present_without_outputs_or_pull_and_not_duplicated() {
        let base = advertised(false, &[]);
        assert_eq!(
            base.iter()
                .filter(|c| c.as_str() == "git_auth_failure")
                .count(),
            1
        );
        assert!(!base
            .iter()
            .any(|c| c == "outputs" || c == "lease_git_auth_failure"));
        let with_pull = advertised(true, PULL_CAPABILITIES);
        let mut expected = CAPABILITIES
            .iter()
            .filter(|c| enabled(c))
            .filter(|c| cfg!(any(unix, windows)) || **c != "session_git_auth")
            .filter(|c| **c != crate::data::workspace::READ_ONLY_WORKSPACE_CAPABILITY)
            .map(|c| c.to_string())
            .collect::<Vec<_>>();
        expected.extend(
            PULL_CAPABILITIES
                .iter()
                .filter(|c| enabled(c))
                .map(|c| c.to_string()),
        );
        assert_eq!(with_pull, expected);
    }

    #[test]
    fn work_item_capabilities_are_static_and_leave_the_lease_one_alone() {
        let base = advertised(false, &[]);
        for flag in ["inline_work_item", "work_item_sources"] {
            assert_eq!(
                base.iter().filter(|c| c.as_str() == flag).count(),
                1,
                "{flag}"
            );
        }
        // The push capability is not the WSP lease capability.
        assert!(!base.iter().any(|c| c == "lease_inline_work_item"));
        assert!(advertised(true, PULL_CAPABILITIES)
            .iter()
            .any(|c| c == "lease_inline_work_item"));
        assert_eq!(
            PULL_CAPABILITIES
                .iter()
                .filter(|c| **c == "lease_inline_work_item")
                .count(),
            1
        );
    }

    #[test]
    fn push_retry_flags_follow_auth_once_and_independent_of_later_slices() {
        let base = advertised(false, &[]);
        let at = |flag: &str| {
            assert_eq!(base.iter().filter(|c| c.as_str() == flag).count(), 1);
            base.iter().position(|c| c == flag).unwrap()
        };
        assert!(at("git_auth_failure") < at("push_retry_events"));
        assert!(at("push_retry_events") < at("push_retry_codes"));
    }

    #[test]
    fn credential_and_secret_flags_follow_push_retry_once() {
        let base = advertised(false, &[]);
        let at = |flag: &str| base.iter().position(|c| c == flag);
        assert_eq!(
            base.iter()
                .filter(|c| c.as_str() == "command_secrets")
                .count(),
            1
        );
        assert!(at("push_retry_codes") < at("command_secrets"));
        if cfg!(any(unix, windows)) {
            assert!(at("push_retry_codes") < at("session_git_auth"));
            assert!(at("session_git_auth") < at("command_secrets"));
        } else {
            assert_eq!(at("session_git_auth"), None);
        }
        // WSP keeps 0143's lease credential capability name.
        assert!(PULL_CAPABILITIES.contains(&"lease_credentials"));
    }
}

#[cfg(test)]
mod read_only_workspace_tests {
    use super::*;
    use crate::data::workspace::READ_ONLY_WORKSPACE_CAPABILITY as RO;

    fn count(list: &[String], flag: &str) -> usize {
        list.iter().filter(|c| c.as_str() == flag).count()
    }

    #[test]
    fn read_only_workspace_is_advertised_once_only_when_the_runtime_enforces_it() {
        assert_eq!(RO, "read_only_workspace");
        assert_eq!(count(&advertised_with(true, true, &[]), RO), 1);
        assert_eq!(count(&advertised_with(true, false, &[]), RO), 0);
        // The legacy entry point never advertises it.
        assert_eq!(count(&advertised(true, PULL_CAPABILITIES), RO), 0);
        // It is the last static capability, before any pull capability.
        let with_pull = advertised_with(false, true, PULL_CAPABILITIES);
        let at = |flag: &str| with_pull.iter().position(|c| c == flag).unwrap();
        assert!(at("work_item_sources") < at(RO));
        assert!(at(RO) < at("worker_pull"));
    }

    #[test]
    fn lease_read_only_stays_a_pull_capability_independent_of_the_runtime() {
        assert!(PULL_CAPABILITIES.contains(&"lease_read_only"));
        assert!(!CAPABILITIES.contains(&"lease_read_only"));
        let without_pull = advertised_with(true, true, &[]);
        assert_eq!(count(&without_pull, "lease_read_only"), 0);
        let pull_only = advertised_with(true, false, &["lease_read_only"]);
        assert_eq!(count(&pull_only, "lease_read_only"), 1);
        assert_eq!(count(&pull_only, RO), 0);
    }
}

#[cfg(test)]
mod image_inputs_tests {
    use super::*;

    fn runtime(
        acquisition: crate::engine::agent_runtime::ImageAcquisition,
    ) -> crate::engine::agent_runtime::Capabilities {
        crate::engine::agent_runtime::Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: acquisition != crate::engine::agent_runtime::ImageAcquisition::Kit,
            image_acquisition: acquisition,
            per_launch_vm_settings: false,
            reports_resource_stats: false,
        }
    }

    #[test]
    fn api_ready_is_advertised_and_image_inputs_only_on_build_runtimes() {
        use crate::engine::agent_runtime::ImageAcquisition::{Build, Import, Kit};
        for acquisition in [Build, Import, Kit] {
            let caps = advertised_for_runtime(&runtime(acquisition), &[]);
            assert!(caps.iter().any(|c| c == "api_ready"));
            assert_eq!(
                caps.iter().any(|c| c == "image_inputs"),
                acquisition == Build,
                "{acquisition:?}: {caps:?}"
            );
        }
    }
}

#[cfg(test)]
mod credential_enrollment_tests {
    #[test]
    fn credential_features_stay_unadvertised_until_production_enrollment_exists() {
        for feature in [
            "agent_credential_updates",
            "agent_credentials_changed",
            "agent_execution_replaced",
        ] {
            assert!(!super::advertised(true, super::PULL_CAPABILITIES)
                .iter()
                .any(|c| c == feature));
        }
    }
}
