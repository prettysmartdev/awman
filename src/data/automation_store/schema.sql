CREATE TABLE IF NOT EXISTS agent_executions (
 execution_id TEXT PRIMARY KEY,
 command_id TEXT NOT NULL REFERENCES commands(id),
 attempt INTEGER NOT NULL CHECK(attempt BETWEEN 1 AND 4294967295),
 owner_key TEXT NOT NULL,
 execution_ordinal INTEGER NOT NULL CHECK(execution_ordinal BETWEEN 1 AND 9007199254740991),
 terminal_id TEXT,
 provider TEXT NOT NULL,
 runtime TEXT NOT NULL,
 provider_version TEXT,
 image_digest TEXT,
 profile_binding_json TEXT NOT NULL,
 requested_native_session_id TEXT,
 native_session_id TEXT,
 automation_mode TEXT CHECK(automation_mode IN ('auto','require_full')),                    -- NULL for ordinary profile runs
 minimum_status_support TEXT CHECK(minimum_status_support IN ('full','best_effort')),
 native_start_mode TEXT NOT NULL CHECK(native_start_mode IN ('default','plan','read_only','custom')),
 workspace_access TEXT NOT NULL CHECK(workspace_access IN ('read_write','read_only')),
 timeouts_json TEXT NOT NULL,
 snapshot_json TEXT,                      -- AutomationSnapshot, no duplicate journal
 revision INTEGER NOT NULL DEFAULT 0 CHECK(revision BETWEEN 0 AND 9007199254740991),
 active_turn_id TEXT,
 launch_phase TEXT NOT NULL CHECK(launch_phase IN ('reserved','launching','running','ended')),              -- reserved|launching|running|ended
 process_started_at TEXT,
 created_at TEXT NOT NULL,
 ended_at TEXT,
 end_failure_json TEXT,
 final_assurance_json TEXT,
 pending_events_json TEXT,                -- bounded existing-log projection batch
 pending_event_cursor TEXT,
 CHECK((pending_events_json IS NULL) = (pending_event_cursor IS NULL)),
 CHECK((launch_phase='ended') = (ended_at IS NOT NULL)),
 CHECK((automation_mode IS NULL) = (snapshot_json IS NULL)),
 UNIQUE(command_id, execution_ordinal)
);
CREATE INDEX IF NOT EXISTS agent_executions_command
 ON agent_executions(command_id, execution_ordinal);
CREATE TABLE IF NOT EXISTS agent_turns (
 turn_id TEXT PRIMARY KEY,
 command_id TEXT NOT NULL REFERENCES commands(id),
 execution_id TEXT NOT NULL REFERENCES agent_executions(execution_id),
 owner_key TEXT NOT NULL,
 request_id TEXT NOT NULL,
 ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 1 AND 9007199254740991),
 attempt INTEGER NOT NULL CHECK(attempt BETWEEN 1 AND 4294967295),
 accepted_revision INTEGER NOT NULL CHECK(accepted_revision BETWEEN 0 AND 9007199254740991),
 admission_support TEXT NOT NULL CHECK(admission_support IN ('full','best_effort','none')),
 admission_source TEXT NOT NULL CHECK(admission_source IN ('native','osc7501','none')),
 native_session_id TEXT,
 native_turn_id TEXT,
 accepted_at TEXT NOT NULL,
 started_at TEXT,
 completed_at TEXT,
 delivery_phase TEXT NOT NULL CHECK(delivery_phase IN ('reserved','write_started','written','acknowledged','terminal')),            -- reserved|write_started|written|acknowledged|terminal
 result_json TEXT,
 CHECK((result_json IS NULL AND completed_at IS NULL AND delivery_phase != 'terminal') OR (result_json IS NOT NULL AND completed_at IS NOT NULL AND delivery_phase = 'terminal')),
 UNIQUE(command_id, ordinal),
 UNIQUE(owner_key, execution_id, request_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS agent_turns_one_pending
 ON agent_turns(execution_id) WHERE result_json IS NULL;
CREATE TABLE IF NOT EXISTS agent_request_dedup (
 owner_key TEXT NOT NULL,
 execution_id TEXT NOT NULL REFERENCES agent_executions(execution_id),
 request_id TEXT NOT NULL,
 command_id TEXT NOT NULL REFERENCES commands(id),
 operation TEXT NOT NULL CHECK(operation IN ('submit_turn','close_execution','credential_update')),                 -- submit_turn|close_execution
 operation_digest TEXT,
 response_json TEXT NOT NULL,             -- typed stable acceptance or refusal
 result_json TEXT,                        -- final turn/close response, once
 created_at TEXT NOT NULL,
 completed_at TEXT,
 CHECK((result_json IS NULL AND completed_at IS NULL) OR (result_json IS NOT NULL AND completed_at IS NOT NULL)),
 PRIMARY KEY(owner_key, execution_id, request_id)
);
CREATE INDEX IF NOT EXISTS agent_request_dedup_command
 ON agent_request_dedup(command_id);

CREATE UNIQUE INDEX IF NOT EXISTS agent_close_one_pending ON agent_request_dedup(execution_id) WHERE operation='close_execution' AND result_json IS NULL;
