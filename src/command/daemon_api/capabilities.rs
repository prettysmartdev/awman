pub const CAPABILITIES: &[&str] = &[
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
];

/// WI 0143: capabilities of the pull worker, in their fixed order. Each is
/// advertised only while the daemon runs a pull worker that reports it
/// (`PullHandle::capabilities`), never speculatively.
pub const PULL_CAPABILITIES: &[&str] = &[
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
        .filter(|c| mounts_outputs || **c != "outputs")
        .chain(PULL_CAPABILITIES.iter().filter(|c| pull.contains(c)))
        .map(|c| c.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_is_listed_once_and_pull_ones_only_when_reported() {
        let all: Vec<&str> = CAPABILITIES
            .iter()
            .chain(PULL_CAPABILITIES)
            .copied()
            .collect();
        let unique: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
        let none = advertised(true, &[]);
        assert!(none
            .iter()
            .all(|c| !PULL_CAPABILITIES.contains(&c.as_str())));
        assert!(none.contains(&"outputs".to_string()));
        assert!(!advertised(false, &[]).contains(&"outputs".to_string()));
        let some = advertised(true, &["lease_read_only", "worker_pull", "bogus"]);
        let tail: Vec<&str> = some[CAPABILITIES.len()..]
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(tail, ["worker_pull", "lease_read_only"]);
    }
}
