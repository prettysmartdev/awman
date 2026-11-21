//! Credential metadata transactions on the existing connection. No plaintext or hashes.
use super::*;
use crate::data::credential_update::*;

fn credential_now() -> chrono::DateTime<Utc> {
    chrono::DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
        .expect("current timestamp")
}

fn slot(c: &Connection, name: &str) -> Result<EffectiveCredentialSlot> {
    let raw: String = c.query_row(
        "SELECT record_json FROM agent_credential_slots WHERE slot=?",
        [name],
        |r| r.get(0),
    )?;
    decode(&raw)
}
fn save_slot(c: &Connection, value: &EffectiveCredentialSlot) -> Result<()> {
    cas(c.execute(
        "UPDATE agent_credential_slots SET record_json=? WHERE slot=?",
        params![json(value)?, value.credential_slot],
    )?)
}
fn operation(
    c: &Connection,
    owner: &str,
    command: &CommandId,
    request: Uuid,
) -> Result<Option<CredentialUpdateOperation>> {
    let raw: Option<String> = c.query_row("SELECT record_json FROM agent_credential_updates WHERE owner_key=? AND command_id=? AND request_id=?",params![owner,command,request.to_string()],|r|r.get(0)).optional()?;
    raw.map(|s| decode(&s)).transpose()
}
fn save_operation(c: &Connection, owner: &str, op: &CredentialUpdateOperation) -> Result<()> {
    cas(c.execute("UPDATE agent_credential_updates SET status=?,record_json=? WHERE owner_key=? AND execution_id=? AND request_id=? AND status IN ('pending','applying','restarting')",params![op.status.as_str(),json(op)?,owner,op.old_execution_id,op.request_id.to_string()])?)
}
fn bump(c: &Connection, id: &ExecutionId, expected: u64) -> Result<()> {
    let mut e = execution(c, id)?;
    let next = next_counter(expected).map_err(invalid)?;
    project(c, id, None)?;
    cas(c.execute(
        "UPDATE agent_executions SET revision=? WHERE execution_id=? AND revision=?",
        params![next as i64, id, expected as i64],
    )?)?;
    // Keep automation projection coherent with the canonical execution revision.
    if let Some(snapshot) = &mut e.snapshot {
        snapshot.revision = next;
        c.execute(
            "UPDATE agent_executions SET snapshot_json=? WHERE execution_id=?",
            params![json(snapshot)?, id],
        )?;
    }
    Ok(())
}
impl AutomationStore {
    /// Durable delivery-started marker, committed before invoking any writer.
    pub fn mark_credential_update_exposed(
        &self,
        owner: &str,
        command: &CommandId,
        request: Uuid,
    ) -> Result<()> {
        let c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        cas(c.execute("UPDATE agent_credential_updates SET exposed=1 WHERE owner_key=? AND command_id=? AND request_id=? AND status IN ('pending','applying','restarting')", params![owner, command, request.to_string()])?)
    }

    /// Stable random identity for a non-secret, already authorized source key.
    pub fn credential_authority(
        &self,
        owner: &str,
        source_key: &str,
        source_ref: &str,
    ) -> Result<Uuid> {
        if source_key.len() != 64
            || !source_key.bytes().all(|b| b.is_ascii_hexdigit())
            || source_ref.len() > 128
        {
            return Err(invalid("invalid source reference"));
        }
        let c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        c.execute("INSERT OR IGNORE INTO agent_credential_authorities(authority_id,owner_key,source_key,source_ref) VALUES(?,?,?,?)",params![Uuid::new_v4().to_string(),owner,source_key,source_ref])?;
        let id:String=c.query_row("SELECT authority_id FROM agent_credential_authorities WHERE owner_key=? AND source_key=?",params![owner,source_key],|r|r.get(0))?;
        Uuid::parse_str(&id).map_err(|_| invalid("invalid authority"))
    }
    /// Initial generation is allocated, never acknowledged by launch alone.
    pub fn register_credential_slot(
        &self,
        owner: &str,
        id: &ExecutionId,
        authority: Uuid,
        mut value: EffectiveCredentialSlot,
    ) -> Result<EffectiveCredentialSlot> {
        let mut c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let e = execution(&tx, id)?;
        if e.owner_key != owner || e.ended_at.is_some() {
            return Err(invalid("not_found"));
        }
        let authority_owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_credential_authorities WHERE authority_id=? AND owner_key=?)",params![authority.to_string(),owner],|r|r.get(0))?;
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM agent_credential_slots WHERE execution_id=?",
            [id],
            |r| r.get(0),
        )?;
        if !authority_owned || count >= 32 || value.secret_names.len() > 32 {
            return Err(invalid("input_backpressure"));
        }
        value.secret_names.sort();
        value.secret_names.dedup();
        for name in &value.secret_names {
            crate::data::secret::SecretName::parse(name).map_err(|_| invalid("invalid_secrets"))?;
        }
        value.credential_slot = format!("cred_{}", Uuid::new_v4());
        value.credential_generation = 0;
        value.pending_generation = Some(1);
        value.generation_state = GenerationState::Unknown;
        value.latest_update = None;
        tx.execute("INSERT INTO agent_credential_slots(slot,command_id,execution_id,authority_id,source_owner,high_water,record_json) VALUES(?,?,?,?,?,1,?)",params![value.credential_slot,e.command_id,id,authority.to_string(),enum_text(&value.route.refresh_owner)?,json(&value)?])?;
        tx.commit()?;
        Ok(value)
    }
    pub fn effective_credentials(
        &self,
        owner: &str,
        id: &ExecutionId,
    ) -> Result<EffectiveCredentialUpdates> {
        let mut c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let e = execution(&tx, id)?;
        if e.owner_key != owner {
            return Err(invalid("not_found"));
        }
        let mut q = tx.prepare(
            "SELECT record_json FROM agent_credential_slots WHERE execution_id=? ORDER BY slot",
        )?;
        let rows = q.query_map([id], |r| r.get::<_, String>(0))?;
        let slots = rows.map(|r| decode(&r?)).collect::<Result<Vec<_>>>()?;
        drop(q);
        tx.commit()?;
        Ok(EffectiveCredentialUpdates {
            execution_id: id.clone(),
            revision: e.revision,
            slots,
        })
    }
    /// Dedup precedes all current-state checks, including replacement/grant changes.
    pub fn lookup_credential_update(
        &self,
        owner: &str,
        command: &CommandId,
        intent: &CredentialUpdateIntent,
    ) -> Result<Option<CredentialUpdateOperation>> {
        let c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let saved:Option<(String,String)>=c.query_row("SELECT command_id,intent_json FROM agent_credential_updates WHERE owner_key=? AND command_id=? AND request_id=?",params![owner,command,intent.request_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((cmd, raw)) = saved {
            if cmd != command.as_str() || decode::<CredentialUpdateIntent>(&raw)? != *intent {
                return Err(invalid("idempotency_conflict"));
            }
            return operation(&c, owner, command, intent.request_id);
        }
        let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM agent_request_dedup WHERE owner_key=? AND execution_id=? AND request_id=?)",params![owner,intent.execution_id,intent.request_id.to_string()],|r|r.get(0))?;
        if exists {
            return Err(invalid("idempotency_conflict"));
        }
        Ok(None)
    }
    pub fn get_credential_update(
        &self,
        owner: &str,
        command: &CommandId,
        request: Uuid,
    ) -> Result<Option<CredentialUpdateOperation>> {
        let c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        operation(&c, owner, command, request)
    }
    pub fn reserve_credential_update(
        &self,
        owner: &str,
        command: &CommandId,
        intent: &CredentialUpdateIntent,
        delay_ms: u64,
    ) -> Result<CredentialUpdateOperation> {
        self.reserve_credential_update_snapshot(owner, command, intent, delay_ms, None)
    }
    pub fn reserve_credential_update_snapshot(
        &self,
        owner: &str,
        command: &CommandId,
        intent: &CredentialUpdateIntent,
        delay_ms: u64,
        snapshot: Option<&AutomationSnapshot>,
    ) -> Result<CredentialUpdateOperation> {
        if delay_ms > MAX_SAFE_COUNTER {
            return Err(invalid("invalid_request"));
        }
        if let Some(op) = self.lookup_credential_update(owner, command, intent)? {
            return Ok(op);
        }
        let mut c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Recheck dedup under the write transaction: another connection may
        // have accepted an equal request between the lookup and BEGIN.
        let saved:Option<(String,String)>=tx.query_row("SELECT command_id,intent_json FROM agent_credential_updates WHERE owner_key=? AND command_id=? AND request_id=?",params![owner,command,intent.request_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((saved_command, raw)) = saved {
            if saved_command != command.as_str()
                || decode::<CredentialUpdateIntent>(&raw)? != *intent
            {
                return Err(invalid("idempotency_conflict"));
            }
            return operation(&tx, owner, command, intent.request_id)?
                .ok_or_else(|| invalid("not_found"));
        }
        let foreign:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_request_dedup WHERE owner_key=? AND execution_id=? AND request_id=?)",params![owner,intent.execution_id,intent.request_id.to_string()],|r|r.get(0))?;
        if foreign {
            return Err(invalid("idempotency_conflict"));
        }
        let e = execution(&tx, &intent.execution_id)?;
        if e.owner_key != owner || e.command_id != *command {
            return Err(invalid("not_found"));
        }
        let current:String=tx.query_row("SELECT execution_id FROM agent_executions WHERE command_id=? ORDER BY execution_ordinal DESC LIMIT 1",[command],|r|r.get(0))?;
        if current != intent.execution_id.as_str() {
            return Err(invalid("stale_execution"));
        }
        let writable: bool = tx.query_row(
            "SELECT status='running' AND result IS NULL FROM commands WHERE id=?",
            [command],
            |r| r.get(0),
        )?;
        if e.ended_at.is_some() || !writable {
            return Err(invalid("input_not_allowed"));
        }
        if e.revision != intent.expected_revision {
            return Err(invalid("stale_revision"));
        }
        let closing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_request_dedup WHERE execution_id=? AND operation='close_execution')",[&intent.execution_id],|r|r.get(0))?;
        let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_credential_updates WHERE COALESCE(json_extract(record_json,'$.new_execution_id'),execution_id)=? AND status IN ('pending','applying','restarting'))",[&intent.execution_id],|r|r.get(0))?;
        if closing || active {
            return Err(invalid("input_not_allowed"));
        }
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM agent_credential_updates WHERE command_id=?",
            [command],
            |r| r.get(0),
        )?;
        if count >= 4096 {
            return Err(invalid("input_backpressure"));
        }
        let mut value = slot(&tx, &intent.credential_slot)?;
        let (binding, high): (String, i64) = tx.query_row(
            "SELECT execution_id,high_water FROM agent_credential_slots WHERE slot=?",
            [&intent.credential_slot],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if binding != e.execution_id.as_str() {
            return Err(invalid("stale_execution"));
        }
        if value.credential_generation != intent.expected_credential_generation {
            return Err(invalid("stale_revision"));
        }
        let availability = value
            .available_operation
            .as_ref()
            .ok_or_else(|| invalid("native_capability_unavailable"))?;
        if intent.reread_source {
            if !availability.reread_source || !intent.secret_names.is_empty() {
                return Err(invalid("invalid_request"));
            }
        } else if !availability.replacement_secrets || intent.secret_names != value.secret_names {
            return Err(invalid("invalid_secrets"));
        }
        match value.update_method {
            CredentialUpdateMethod::Unknown => return Err(invalid("agent_not_ready")),
            CredentialUpdateMethod::Unavailable => {
                return Err(invalid("native_capability_unavailable"))
            }
            CredentialUpdateMethod::Restart
                if value.route.restart_supported != Some(true)
                    || value.route.conversation_continuity
                        != Some(ConversationContinuity::NativeResume) =>
            {
                return Err(invalid("native_capability_unavailable"))
            }
            _ => {}
        }
        let generation = next_counter(high as u64).map_err(invalid)?;
        let now = credential_now();
        let op = CredentialUpdateOperation {
            request_id: intent.request_id,
            command_id: command.clone(),
            credential_slot: intent.credential_slot.clone(),
            status: CredentialUpdateStatus::Pending,
            method: value.update_method,
            old_execution_id: e.execution_id.clone(),
            new_execution_id: None,
            old_generation: value.credential_generation,
            new_generation: generation,
            pending_generation: Some(generation),
            credential_generation: value.credential_generation,
            generation_state: value.generation_state,
            refresh_owner: if intent.reread_source {
                let saved: String = tx.query_row(
                    "SELECT source_owner FROM agent_credential_slots WHERE slot=?",
                    [&intent.credential_slot],
                    |r| r.get(0),
                )?;
                decode(&json(&saved)?)?
            } else {
                RefreshOwner::Caller
            },
            conversation_continuity: value.route.conversation_continuity,
            restart_scope: value.route.restart_scope,
            reason: None,
            failure: None,
            created_at: now,
            apply_deadline: now
                .checked_add_signed(
                    chrono::Duration::try_milliseconds(delay_ms as i64)
                        .ok_or_else(|| invalid("invalid_request"))?,
                )
                .ok_or_else(|| invalid("invalid_request"))?,
            completed_at: None,
        };
        value.pending_generation = Some(generation);
        value.admission_while_pending = false;
        value.latest_update = Some(op.clone());
        value.route.refresh_owner = op.refresh_owner;
        save_slot(&tx, &value)?;
        cas(tx.execute(
            "UPDATE agent_credential_slots SET high_water=? WHERE slot=? AND high_water=?",
            params![generation as i64, intent.credential_slot, high],
        )?)?;
        tx.execute("INSERT INTO agent_credential_updates(owner_key,execution_id,request_id,command_id,slot,intent_json,status,record_json) VALUES(?,?,?,?,?,?,'pending',?)",params![owner,e.execution_id,intent.request_id.to_string(),command,intent.credential_slot,json(intent)?,json(&op)?])?;
        tx.execute("INSERT INTO agent_request_dedup(owner_key,execution_id,request_id,command_id,operation,operation_digest,response_json,created_at) VALUES(?,?,?,?,'credential_update',NULL,?,?)",params![owner,e.execution_id,intent.request_id.to_string(),command,json(&StoredReply::CredentialUpdate)?,now.to_rfc3339()])?;
        if let Some(snapshot) = snapshot {
            transition(&tx, e.revision, snapshot, None)?;
        } else {
            bump(&tx, &e.execution_id, e.revision)?;
        }
        tx.commit()?;
        Ok(op)
    }
    /// Staging only permits admission once the adapter establishes next-request resolution.
    pub fn stage_credential_generation(
        &self,
        owner: &str,
        command: &CommandId,
        request: Uuid,
        admission: bool,
        snapshot: Option<&AutomationSnapshot>,
    ) -> Result<CredentialUpdateOperation> {
        self.change_credentials(owner, command, request, true, snapshot, |op, value| {
            op.status = if op.method == CredentialUpdateMethod::Restart {
                CredentialUpdateStatus::Restarting
            } else {
                CredentialUpdateStatus::Applying
            };
            op.reason = Some(AWAITING_NEXT_REQUEST_REASON.into());
            value.admission_while_pending = admission;
            Ok(())
        })
    }
    /// Only a current execution's generation-bound native consumption may call this.
    pub fn ack_credential_generation(
        &self,
        owner: &str,
        command: &CommandId,
        request: Uuid,
        execution_id: &ExecutionId,
        generation: u64,
        snapshot: Option<&AutomationSnapshot>,
    ) -> Result<CredentialUpdateOperation> {
        self.change_credentials(owner, command, request, true, snapshot, |op, value| {
            if (op.method == CredentialUpdateMethod::Restart
                && op.new_execution_id.as_ref() != Some(execution_id))
                || op.new_execution_id.as_ref().unwrap_or(&op.old_execution_id) != execution_id
                || generation != op.new_generation
                || credential_now() >= op.apply_deadline
            {
                return Err(invalid("stale_execution"));
            }
            op.status = CredentialUpdateStatus::Applied;
            op.credential_generation = generation;
            op.pending_generation = None;
            op.generation_state = GenerationState::Confirmed;
            op.reason = None;
            op.completed_at = Some(credential_now());
            value.credential_generation = generation;
            value.pending_generation = None;
            value.generation_state = GenerationState::Confirmed;
            value.update_method = value.route.method(value.generation_state);
            value.admission_while_pending = false;
            Ok(())
        })
    }
    pub fn fail_credential_update(
        &self,
        owner: &str,
        command: &CommandId,
        request: Uuid,
        failure: CredentialUpdateFailure,
        snapshot: Option<&AutomationSnapshot>,
    ) -> Result<CredentialUpdateOperation> {
        let CredentialUpdateFailure {
            code,
            reason,
            exposed,
        } = failure;
        let code = if code.len() <= 128
            && code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            code
        } else {
            "credential_update_failed"
        };
        let reason = if reason.len() <= 1024
            && reason
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            reason
        } else {
            "credential_update_failed"
        };
        self.change_credentials(owner, command, request, false, snapshot, |op, value| {
            op.status = CredentialUpdateStatus::Failed;
            op.reason = Some(reason.into());
            op.failure = Some(AutomationFailure {
                code: code.into(),
                message: reason.into(),
            });
            op.completed_at = Some(credential_now());
            op.pending_generation = None;
            value.pending_generation = None;
            value.admission_while_pending = false;
            if exposed {
                op.generation_state = GenerationState::Unknown;
                value.generation_state = GenerationState::Unknown;
                value.update_method = value.route.method(value.generation_state);
            }
            Ok(())
        })
    }
    fn change_credentials(
        &self,
        owner: &str,
        command: &CommandId,
        request: Uuid,
        require_live: bool,
        snapshot: Option<&AutomationSnapshot>,
        change: impl FnOnce(&mut CredentialUpdateOperation, &mut EffectiveCredentialSlot) -> Result<()>,
    ) -> Result<CredentialUpdateOperation> {
        let mut c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut op =
            operation(&tx, owner, command, request)?.ok_or_else(|| invalid("not_found"))?;
        if op.status.is_terminal() {
            return Err(invalid("input_not_allowed"));
        }
        let current_id = op
            .new_execution_id
            .as_ref()
            .unwrap_or(&op.old_execution_id)
            .clone();
        let current = execution(&tx, &current_id)?;
        if require_live && current.ended_at.is_some() {
            return Err(invalid("input_not_allowed"));
        }
        let mut value = slot(&tx, &op.credential_slot)?;
        if value.pending_generation != Some(op.new_generation) {
            return Err(invalid("stale_revision"));
        }
        change(&mut op, &mut value)?;
        value.latest_update = Some(op.clone());
        save_slot(&tx, &value)?;
        save_operation(&tx, owner, &op)?;
        if let Some(snapshot) = snapshot {
            if snapshot.execution_id != current_id {
                return Err(invalid("stale_execution"));
            }
            transition(&tx, current.revision, snapshot, None)?;
        } else {
            bump(&tx, &current_id, current.revision)?;
        }
        tx.commit()?;
        Ok(op)
    }
    pub fn recover_credential_updates(&self) -> Result<()> {
        let pending: Vec<(String, CommandId, Uuid, ExecutionId, bool)> = {
            let c = self
                .conn
                .lock()
                .map_err(|_| invalid("database lock poisoned"))?;
            let mut q=c.prepare("SELECT owner_key,command_id,request_id,COALESCE(json_extract(record_json,'$.new_execution_id'),execution_id),exposed FROM agent_credential_updates WHERE status IN ('pending','applying','restarting')")?;
            let rows = q
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get::<_, String>(2)?,
                        r.get(3)?,
                        r.get(4)?,
                    ))
                })?
                .map(|r| {
                    let (o, c, id, e, exposed) = r?;
                    Ok((
                        o,
                        c,
                        Uuid::parse_str(&id).map_err(|_| invalid("invalid request"))?,
                        e,
                        exposed,
                    ))
                })
                .collect::<Result<_>>()?;
            rows
        };
        for (owner, command, request, id, exposed) in pending {
            if !self.lease_held(&id)? {
                self.fail_credential_update(
                    &owner,
                    &command,
                    request,
                    CredentialUpdateFailure {
                        code: "credentials_lost",
                        reason: "credentials_lost",
                        exposed,
                    },
                    None,
                )?;
            }
        }
        Ok(())
    }
}

impl AutomationStore {
    /// Transfer stable slots only after an owned replacement has its own durable
    /// record. Old observations remain fenced by execution identity/generation.
    pub fn bind_replacement_credentials(
        &self,
        owner: &str,
        command: &CommandId,
        request: Uuid,
        new_id: &ExecutionId,
        snapshot: Option<&AutomationSnapshot>,
    ) -> Result<CredentialUpdateOperation> {
        let mut c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut op =
            operation(&tx, owner, command, request)?.ok_or_else(|| invalid("not_found"))?;
        if op.status.is_terminal()
            || op.method != CredentialUpdateMethod::Restart
            || op.new_execution_id.is_some()
        {
            return Err(invalid("input_not_allowed"));
        }
        let old = execution(&tx, &op.old_execution_id)?;
        let new = execution(&tx, new_id)?;
        if old.ended_at.is_none()
            || new.ended_at.is_some()
            || new.owner_key != owner
            || new.command_id != *command
            || new.profile_binding != old.profile_binding
            || new.attempt != old.attempt
            || new.provider != old.provider
            || new.runtime != old.runtime
            || new.automation_mode != old.automation_mode
            || new.minimum_status_support != old.minimum_status_support
            || new.timeouts != old.timeouts
            || new.requested_native_session_id != old.native_session_id
            || new.native_session_id != old.native_session_id
            || old.native_session_id.is_none()
            || new.native_start_mode != old.native_start_mode
            || new.workspace_access != old.workspace_access
            || new.execution_ordinal <= old.execution_ordinal
        {
            return Err(invalid("native_capability_unavailable"));
        }
        op.new_execution_id = Some(new_id.clone());
        op.status = CredentialUpdateStatus::Restarting;
        let mut value = slot(&tx, &op.credential_slot)?;
        value.latest_update = Some(op.clone());
        save_slot(&tx, &value)?;
        tx.execute("UPDATE agent_credential_slots SET execution_id=? WHERE command_id=? AND execution_id=?",params![new_id,command,old.execution_id])?;
        save_operation(&tx, owner, &op)?;
        if let Some(snapshot) = snapshot {
            if snapshot.execution_id != *new_id {
                return Err(invalid("stale_execution"));
            }
            transition(&tx, new.revision, snapshot, None)?;
        } else {
            bump(&tx, new_id, new.revision)?;
        }
        tx.commit()?;
        Ok(op)
    }
}

impl AutomationStore {
    /// Initial staging is not an acknowledgement. Bind consumption to the
    /// still-current generation 1; any later allocation permanently fences it.
    pub fn ack_initial_credential_generation(
        &self,
        owner: &str,
        id: &ExecutionId,
        name: &str,
        snapshot: Option<&AutomationSnapshot>,
    ) -> Result<()> {
        let mut c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let e = execution(&tx, id)?;
        if e.owner_key != owner || e.ended_at.is_some() {
            return Err(invalid("not_found"));
        }
        let (bound, high): (String, i64) = tx.query_row(
            "SELECT execution_id,high_water FROM agent_credential_slots WHERE slot=?",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut value = slot(&tx, name)?;
        if bound != id.as_str()
            || high != 1
            || value.pending_generation != Some(1)
            || value.latest_update.is_some()
        {
            return Err(invalid("stale_revision"));
        }
        value.credential_generation = 1;
        value.pending_generation = None;
        value.generation_state = GenerationState::Confirmed;
        value.admission_while_pending = false;
        save_slot(&tx, &value)?;
        if let Some(snapshot) = snapshot {
            transition(&tx, e.revision, snapshot, None)?;
        } else {
            bump(&tx, id, e.revision)?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl AutomationStore {
    /// Read both projections under one database snapshot, including a command
    /// whose execution identity changed on a different connection.
    pub fn owned_execution_snapshot(
        &self,
        owner: &str,
        command: &CommandId,
    ) -> Result<Option<OwnedExecutionSnapshot>> {
        let mut c = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let id:Option<ExecutionId>=tx.query_row("SELECT execution_id FROM agent_executions WHERE command_id=? ORDER BY execution_ordinal DESC LIMIT 1",[command],|r|r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        let e = execution(&tx, &id)?;
        if e.owner_key != owner {
            return Ok(None);
        }
        let mut q = tx.prepare(
            "SELECT record_json FROM agent_credential_slots WHERE execution_id=? ORDER BY slot",
        )?;
        let rows = q.query_map([&id], |r| r.get::<_, String>(0))?;
        let slots = rows.map(|r| decode(&r?)).collect::<Result<Vec<_>>>()?;
        drop(q);
        tx.commit()?;
        Ok(Some(OwnedExecutionSnapshot {
            automation: e.snapshot,
            credential_updates: Some(EffectiveCredentialUpdates {
                execution_id: id,
                revision: e.revision,
                slots,
            }),
        }))
    }
}
