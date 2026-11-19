CREATE TABLE IF NOT EXISTS agent_credential_authorities (
 authority_id TEXT PRIMARY KEY,
 owner_key TEXT NOT NULL,
 source_key TEXT NOT NULL,
 source_ref TEXT NOT NULL,
 UNIQUE(owner_key,source_key)
);
CREATE TABLE IF NOT EXISTS agent_credential_slots (
 slot TEXT PRIMARY KEY,
 command_id TEXT NOT NULL REFERENCES commands(id),
 execution_id TEXT NOT NULL REFERENCES agent_executions(execution_id),
 authority_id TEXT NOT NULL REFERENCES agent_credential_authorities(authority_id),
 source_owner TEXT NOT NULL CHECK(source_owner IN ('awman','provider','caller','none')),
 high_water INTEGER NOT NULL CHECK(high_water BETWEEN 1 AND 9007199254740991),
 record_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS agent_credential_updates (
 owner_key TEXT NOT NULL,
 execution_id TEXT NOT NULL REFERENCES agent_executions(execution_id),
 request_id TEXT NOT NULL,
 command_id TEXT NOT NULL REFERENCES commands(id),
 slot TEXT NOT NULL REFERENCES agent_credential_slots(slot),
 intent_json TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('pending','applying','restarting','applied','failed')),
 exposed INTEGER NOT NULL DEFAULT 0 CHECK(exposed IN (0,1)),
 record_json TEXT NOT NULL,
 PRIMARY KEY(owner_key,execution_id,request_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS credential_one_active ON agent_credential_updates(execution_id) WHERE status IN ('pending','applying','restarting');
CREATE UNIQUE INDEX IF NOT EXISTS credential_current_execution_active
 ON agent_credential_updates(COALESCE(json_extract(record_json,'$.new_execution_id'),execution_id))
 WHERE status IN ('pending','applying','restarting');
CREATE UNIQUE INDEX IF NOT EXISTS credential_command_request
 ON agent_credential_updates(owner_key,command_id,request_id);
