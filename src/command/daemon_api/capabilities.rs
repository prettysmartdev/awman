pub const CAPABILITIES: &[&str] = &[
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
];

/// WI 0143: capabilities of the pull worker, in their fixed order. Each is
/// advertised only while the daemon runs a pull worker that reports it
/// (`PullHandle::capabilities`), never speculatively.
pub const PULL_CAPABILITIES: &[&str] = &[
    "lease_context",
    "lease_workflow_context",
    "worker_pull",
    "lease_credentials",
    "lease_inline_work_item",
    "lease_read_only",
    "lease_git_auth_failure",
    "lease_push_retry",
];

/// The `capabilities` list of `/v1/status`: every static capability
/// (`outputs` only when the runtime mounts `/awman/output`), then the pull
/// worker's, in [`PULL_CAPABILITIES`] order.
pub fn advertised(mounts_outputs: bool, pull: &[&str]) -> Vec<String> {
    CAPABILITIES
        .iter()
        .filter(|c| enabled(c))
        .filter(|c| mounts_outputs || **c != "outputs")
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
