pub const CAPABILITIES: &[&str] = &[
    "daemon_features",
    "network_listener",
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
    // WI 0137: `GET /v1/commands/{id}/outputs[/{path}]`.
    "outputs",
    // WI 0137: `source` on the event envelope and `?step=` on the logs.
    "event_source",
    // WI 0137: `step_activity` events from structured-output agents.
    "step_activity",
    // WI 0138: `GET /v1/commands/{id}`, the command record.
    "command_record",
    // WI 0138: `setup_status`/`setup_error` on session records and the
    // `?setup_status=` filter on `GET /v1/sessions`.
    "session_setup_status",
    // WI 0138: `session_id` in the `POST /v1/commands` body.
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
    // Not advertised yet (WI 0141): `terminal` and `interactive`. The
    // `/v1/terminals` resource, its `awman.terminal.v1` WebSocket and the
    // input grant are served, but no daemon PTY registers with the terminal
    // hub yet, so every list is empty. Add both here in the change that makes
    // `spawn_pty_bridged` register daemon terminals.
];
