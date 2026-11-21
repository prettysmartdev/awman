//! ACP session lifecycle and portable frontend driver.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Mutex;

/// Deadline for the one-shot ACP handshake requests (`initialize`,
/// `session/new`). These must complete quickly; a peer that answers
/// `initialize` and then never answers `session/new` would otherwise hang awman
/// forever (stdout stays open, so the connection-close path never fires).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

use crate::data::message::UserMessageSink;
use crate::engine::acp::budget::AcpQueueStats;
use crate::engine::acp::client::{
    AcpCancelHandle, AcpClient, AcpIncoming, AcpTransport, AcpUpdates, IncomingRequest,
};
use crate::engine::acp::frontend::AcpFrontend;
use crate::engine::acp::protocol::{
    ContentBlock, InitializeRequest, InitializeResponse, JsonRpcId, NewSessionRequest,
    NewSessionResponse, PermissionDecision, PermissionRequest, PromptRequest, PromptResponse,
    SetSessionModeRequest,
};
use crate::engine::agent_runtime::execution::{AgentExecution, AgentExitInfo};
use crate::engine::error::EngineError;

/// A running ACP agent plus its JSON-RPC client.
///
/// Construct the companion [`crate::engine::acp::AcpTransportFrontend`] with
/// `AcpTransport::channel`, pass it to `AgentInstance::run_with_frontend`, and
/// then build this session from the resulting execution and transport.
/// What an ACP session does when the agent asks permission for a tool call.
///
/// The session is *told* the policy rather than deriving it from `--yolo` /
/// `--auto`: those are command flags, and reading them here put a permission
/// decision — the most security-relevant one awman makes — in Layer 1 (F-46).
/// Layer 2 computes this from the flags it owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionPolicy {
    /// Approve every request without consulting the frontend.
    AutoApprove,
    /// Ask the frontend. The default: a session that has not been told
    /// otherwise must never approve on the user's behalf.
    #[default]
    Ask,
}
pub struct AcpSession {
    execution: AgentExecution,
    client: Arc<AcpClient>,
    session_id: Mutex<Option<String>>,
    permissions_rx: AcpIncoming,
    pending_permissions: Mutex<HashSet<JsonRpcId>>,
    updates_rx: AcpUpdates,
    cancel: AcpCancelHandle,
    permissions: PermissionPolicy,
    /// The starting mode `initialize` was asked to establish, if any.
    requested_mode: Option<String>,
    /// The mode the agent acknowledged (`session/new` already in it, or a
    /// successful `session/set_mode`). No prompt is sent while a requested
    /// mode is unacknowledged.
    current_mode: Option<String>,
    /// The `modes` of the `session/new` response, consumed by `initialize`.
    advertised_modes: Option<crate::engine::acp::protocol::SessionModeState>,
    /// Named in a start-mode refusal.
    agent_name: String,
}

impl AcpSession {
    pub fn from_transport(
        execution: AgentExecution,
        transport: AcpTransport,
        sink: Box<dyn UserMessageSink>,
        permissions: PermissionPolicy,
    ) -> Self {
        Self::new(execution, AcpClient::new(transport, sink), permissions)
    }

    pub fn new(
        execution: AgentExecution,
        mut client: AcpClient,
        permissions: PermissionPolicy,
    ) -> Self {
        let permissions_rx = client.take_incoming();
        let updates_rx = client.take_updates();
        let cancel = client.cancel_handle();
        Self {
            execution,
            client: Arc::new(client),
            session_id: Mutex::new(None),
            permissions_rx,
            pending_permissions: Mutex::new(HashSet::new()),
            updates_rx,
            cancel,
            permissions,
            requested_mode: None,
            current_mode: None,
            advertised_modes: None,
            agent_name: "agent".into(),
        }
    }

    /// Negotiate ACP v1 then create a session rooted at the supplied absolute
    /// *container* working directory.
    ///
    /// With `start_mode` (a definition's `acp_mode_id` for `--plan`), the
    /// session must advertise that mode and be in it before this returns:
    /// `session/set_mode` is sent when it is not already current, and its
    /// success response is the acknowledgement. Anything else is a
    /// `NativePlanRouteUnsupported` refusal; the caller shuts the execution
    /// down. No prompt can be sent until the mode is acknowledged.
    pub async fn initialize(
        &mut self,
        cwd: impl AsRef<Path>,
        start_mode: Option<&str>,
    ) -> Result<String, EngineError> {
        self.requested_mode = start_mode.map(str::to_string);
        let session_id = self.initialize_session(cwd.as_ref()).await?;
        if let Some(mode) = start_mode {
            self.establish_mode(&session_id, mode).await?;
        }
        Ok(session_id)
    }

    async fn establish_mode(&mut self, session_id: &str, mode: &str) -> Result<(), EngineError> {
        let agent = self.agent_name.clone();
        let refuse = |reason: String| EngineError::NativePlanRouteUnsupported {
            agent: agent.clone(),
            route: "acp",
            reason,
        };
        let advertised = self.advertised_modes.take();
        let Some(state) = advertised else {
            return Err(refuse(format!(
                "agent does not advertise session mode '{mode}'; use agents.launchMode=stdio or \
                 launch without --plan"
            )));
        };
        if !state.available_modes.iter().any(|m| m.id == mode) {
            return Err(refuse(format!(
                "agent does not advertise session mode '{mode}'; use agents.launchMode=stdio or \
                 launch without --plan"
            )));
        }
        if state.current_mode_id != mode {
            self.handshake_request(
                "session/set_mode",
                SetSessionModeRequest {
                    session_id: session_id.to_string(),
                    mode_id: mode.to_string(),
                },
            )
            .await
            .map_err(|e| {
                refuse(format!(
                    "session/set_mode '{mode}' was not acknowledged: {e}"
                ))
            })?;
        }
        self.current_mode = Some(mode.to_string());
        Ok(())
    }

    /// Name the agent in a start-mode refusal.
    pub fn with_agent_name(mut self, name: impl Into<String>) -> Self {
        self.agent_name = name.into();
        self
    }

    /// The mode the agent acknowledged at start, if one was requested.
    pub fn current_mode(&self) -> Option<&str> {
        self.current_mode.as_deref()
    }

    /// Refuse a prompt while a requested start mode is unacknowledged.
    fn ensure_start_mode(&self) -> Result<(), EngineError> {
        match (&self.requested_mode, &self.current_mode) {
            (Some(_), None) => Err(EngineError::Acp("start mode not established".into())),
            _ => Ok(()),
        }
    }

    async fn initialize_session(&mut self, cwd: &Path) -> Result<String, EngineError> {
        // A container path is POSIX whatever the host is: `Path::is_absolute`
        // rejects "/workspace" on a Windows host.
        if !cwd.to_str().is_some_and(|path| path.starts_with('/')) {
            return Err(EngineError::Acp(
                "ACP session cwd must be absolute inside the container".into(),
            ));
        }
        let initialized: InitializeResponse = decode(
            self.handshake_request("initialize", InitializeRequest::default())
                .await?,
        )?;
        // `initialize` returns the *negotiated* version, which the ACP spec
        // permits to be lower than the one we requested. Accept anything up to
        // and including our supported version; only a higher (unknown) version
        // is a hard error.
        if initialized.protocol_version == 0
            || initialized.protocol_version > crate::engine::acp::protocol::ACP_PROTOCOL_VERSION
        {
            return Err(EngineError::Acp(format!(
                "unsupported ACP protocol version {}",
                initialized.protocol_version
            )));
        }
        let created: NewSessionResponse = decode(
            self.handshake_request(
                "session/new",
                NewSessionRequest {
                    cwd: cwd.to_string_lossy().into_owned(),
                    mcp_servers: vec![],
                    additional_directories: vec![],
                },
            )
            .await?,
        )?;
        *self.session_id.lock().await = Some(created.session_id.clone());
        self.advertised_modes = created.modes;
        Ok(created.session_id)
    }

    /// A handshake JSON-RPC request bounded by [`HANDSHAKE_TIMEOUT`]. Takes
    /// `&mut self` so the future stays `Send` (a shared `&AcpSession` held
    /// across `.await` would require `AcpSession: Sync`, which the command
    /// futures deliberately avoid).
    async fn handshake_request<T: Serialize>(
        &mut self,
        method: &str,
        params: T,
    ) -> Result<serde_json::Value, EngineError> {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.client.request(method, params)).await {
            Ok(result) => result,
            Err(_) => Err(EngineError::Acp(format!(
                "timed out after {}s waiting for ACP {method} response",
                HANDSHAKE_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Cancel the agent process and reap it, returning its exit info. Call on
    /// any error path once the container has launched, so the `docker run`
    /// child is never leaked (an errored `initialize`/`prompt` otherwise drops
    /// the still-running execution without waiting on it).
    ///
    /// Never waits for stdin space: an agent that stopped reading cannot hold
    /// up teardown.
    pub async fn shutdown(&mut self) -> Result<AgentExitInfo, EngineError> {
        let _ = self.cancel_now().await;
        self.cancel.cancel();
        let _ = self.execution.cancel();
        self.execution.wait().await
    }

    pub async fn prompt(&mut self, text: impl Into<String>) -> Result<PromptResponse, EngineError> {
        self.ensure_start_mode()?;
        let session_id = self
            .session_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| EngineError::Acp("ACP session has not been initialized".into()))?;
        decode(
            self.client
                .request(
                    "session/prompt",
                    PromptRequest {
                        session_id,
                        prompt: vec![ContentBlock::Text { text: text.into() }],
                    },
                )
                .await?,
        )
    }

    /// Interrupt the active prompt and mark all still-pending permission
    /// requests cancelled, as required by ACP's cancellation semantics.
    pub async fn cancel(&mut self) -> Result<(), EngineError> {
        if let Some(session_id) = self.session_id.lock().await.clone() {
            self.client
                .notify(
                    "session/cancel",
                    serde_json::json!({"sessionId": session_id}),
                )
                .await?;
        }
        let ids: Vec<_> = self.pending_permissions.lock().await.drain().collect();
        for id in ids {
            self.client
                .respond_permission(id, PermissionDecision::Cancelled)
                .await?;
        }
        Ok(())
    }

    /// [`cancel`](Self::cancel) for teardown: sends only what fits in the
    /// stdin budget right now.
    async fn cancel_now(&mut self) -> Result<(), EngineError> {
        if let Some(session_id) = self.session_id.lock().await.clone() {
            self.client.notify_now(
                "session/cancel",
                serde_json::json!({"sessionId": session_id}),
            )?;
        }
        let ids: Vec<_> = self.pending_permissions.lock().await.drain().collect();
        for id in ids {
            self.client
                .respond_now(id, PermissionDecision::Cancelled.as_result())?;
        }
        Ok(())
    }

    /// Send the response for exactly the agent request identified by `request_id`.
    pub async fn respond_permission(
        &mut self,
        request_id: JsonRpcId,
        decision: PermissionDecision,
    ) -> Result<(), EngineError> {
        self.pending_permissions.lock().await.remove(&request_id);
        self.client.respond_permission(request_id, decision).await
    }

    /// A handle that cancels this session's transport from another task:
    /// reader tasks stop, queued data is freed and pending requests fail.
    /// A driver blocked in [`drive`](Self::drive) then returns.
    pub fn cancel_handle(&self) -> AcpCancelHandle {
        self.cancel.clone()
    }

    /// Occupancy and high-water marks of every ACP queue.
    pub fn queue_stats(&self) -> AcpQueueStats {
        self.cancel.stats()
    }

    pub fn execution(&self) -> &AgentExecution {
        &self.execution
    }
    pub async fn wait(&mut self) -> Result<AgentExitInfo, EngineError> {
        self.execution.wait().await
    }

    /// Reap the execution after the transport ended. An overflow is the
    /// session's result; otherwise the agent's exit info is.
    async fn finish_transport(&mut self) -> Result<AgentExitInfo, EngineError> {
        let _ = self.cancel_now().await;
        self.cancel.cancel();
        let _ = self.execution.cancel();
        let exit = self.execution.wait().await;
        match self.client.failure() {
            Some(overflow) => Err(overflow),
            None => exit,
        }
    }

    /// Drive a complete interactive conversation through the portable
    /// frontend. The only calls made into presentation code are the three
    /// methods on `AcpFrontend`.
    pub async fn drive(
        &mut self,
        frontend: &mut dyn AcpFrontend,
    ) -> Result<AgentExitInfo, EngineError> {
        self.drive_with_initial_prompt(frontend, None).await
    }

    /// Like [`drive`](Self::drive) but runs `initial` as the first turn before
    /// asking the frontend for follow-ups. The update subscription is taken
    /// **before** the first prompt is sent, so no `session/update` of the seeded
    /// turn is lost — a `tokio::broadcast` drops any message published while it
    /// has no subscribers, which previously silenced the entire first turn of a
    /// headless `exec prompt --set agents.launchMode=acp` run. Running the seeded turn
    /// inside the same driver loop also services `session/request_permission`
    /// during that turn, avoiding the permission-before-prompt-response
    /// circular wait.
    ///
    /// Control (permission requests) is polled before bulk updates, and the
    /// driver never waits on one without draining the other, so a saturated
    /// update queue cannot hold back a permission reply or the turn result.
    /// While the frontend is inside a synchronous call the reader applies
    /// backpressure; cancellation through [`cancel_handle`](Self::cancel_handle)
    /// still stops the transport at once.
    pub async fn drive_with_initial_prompt(
        &mut self,
        frontend: &mut dyn AcpFrontend,
        initial: Option<String>,
    ) -> Result<AgentExitInfo, EngineError> {
        self.ensure_start_mode()?;
        let mut next = initial;
        loop {
            let prompt = match next.take() {
                Some(prompt) => prompt,
                None => match frontend.next_prompt() {
                    Some(prompt) => prompt,
                    // `None` means the whole awman session ends, so also use
                    // the normal execution cancellation path instead of
                    // waiting indefinitely for a cooperative agent to exit.
                    None => return self.finish_transport().await,
                },
            };
            if self.cancel.is_cancelled() {
                return self.finish_transport().await;
            }
            let client = self.client.clone();
            let session_id =
                self.session_id.lock().await.clone().ok_or_else(|| {
                    EngineError::Acp("ACP session has not been initialized".into())
                })?;
            let mut turn = Box::pin(async move {
                let value = client
                    .request(
                        "session/prompt",
                        PromptRequest {
                            session_id,
                            prompt: vec![ContentBlock::Text { text: prompt }],
                        },
                    )
                    .await?;
                decode::<PromptResponse>(value)
            });
            loop {
                tokio::select! {
                    biased;
                    request = self.permissions_rx.recv() => match request {
                        Some(IncomingRequest::Permission(request)) => {
                            if let Err(e) = self.handle_permission(frontend, request).await {
                                // The reply could not be sent: the transport
                                // closed or was cancelled. Reap, and report an
                                // overflow if that is what closed it.
                                drop(turn);
                                let closed = self.cancel.is_cancelled();
                                let ended = self.finish_transport().await;
                                return if closed { ended } else { Err(e) };
                            }
                        }
                        // Control closed => the connection ended or was
                        // cancelled. Render what the turn already queued, then
                        // reap — cancel covers a peer that closes stdout but
                        // keeps its process alive.
                        None => {
                            while let Some(update) = self.updates_rx.try_recv() {
                                frontend.render_update(update);
                            }
                            drop(turn);
                            return self.finish_transport().await;
                        }
                    },
                    result = &mut turn => {
                        // The reader enqueues a turn's `session/update`s before
                        // the prompt response that ends the turn, so anything
                        // still queued here belongs to this turn — drain and
                        // render it before finishing (otherwise the tail of a
                        // turn, or the whole of a fast headless turn, is lost).
                        while let Some(update) = self.updates_rx.try_recv() {
                            frontend.render_update(update);
                        }
                        if let Err(e) = result {
                            // The turn RPC failed (typically the agent dropped
                            // the connection mid-turn). Reap the child so it is
                            // never leaked, then surface the error.
                            let _ = self.finish_transport().await;
                            return Err(self.client.failure().unwrap_or(e));
                        }
                        break;
                    }
                    update = self.updates_rx.recv() => match update {
                        Some(update) => frontend.render_update(update),
                        None => {
                            drop(turn);
                            return self.finish_transport().await;
                        }
                    },
                }
            }
        }
    }

    async fn handle_permission(
        &mut self,
        frontend: &mut dyn AcpFrontend,
        request: PermissionRequest,
    ) -> Result<(), EngineError> {
        self.pending_permissions
            .lock()
            .await
            .insert(request.request_id.clone());
        let decision = if matches!(self.permissions, PermissionPolicy::AutoApprove) {
            PermissionDecision::approve(&request.options)
        } else {
            frontend.request_permission(request.clone())
        };
        self.respond_permission(request.request_id, decision).await
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, EngineError> {
    serde_json::from_value(value)
        .map_err(|e| EngineError::Acp(format!("invalid ACP response: {e}")))
}

#[cfg(test)]
mod start_mode_tests {
    //! WI 0070: an ACP `--plan` launch selects and confirms the declared
    //! session mode before the first prompt, through a fake agent speaking
    //! JSON-RPC over the real transport.
    use super::*;
    use crate::data::message::{UserMessage, UserMessageSink};
    use crate::engine::acp::protocol::SessionUpdate;
    use crate::engine::agent_runtime::execution::{CancelHandle, ExecutionBackend};
    use crate::engine::agent_runtime::frontend::AgentFrontend;
    use serde_json::{json, Value};
    use std::sync::Mutex as StdMutex;

    struct Quiet;
    impl UserMessageSink for Quiet {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    struct Done;
    impl ExecutionBackend for Done {
        fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
            let now = chrono::DateTime::from_timestamp(0, 0).unwrap();
            Ok(AgentExitInfo {
                exit_code: 0,
                signal: None,
                started_at: now,
                ended_at: now,
            })
        }
        fn cancel(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn cancel_handle(&self) -> CancelHandle {
            CancelHandle::new(|| Ok(()))
        }
    }

    /// Start a fake ACP agent. `modes` is the `session/new` `modes` value;
    /// `set_mode_error` makes `session/set_mode` fail. Returns the session
    /// and the methods the agent received, in order.
    fn session(
        modes: Option<Value>,
        set_mode_error: bool,
    ) -> (AcpSession, Arc<StdMutex<Vec<String>>>) {
        let (mut frontend, transport) = AcpTransport::channel();
        let io = frontend.take_io();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let stdout = io.stdout;
        let mut stdin = io.stdin_rx;
        tokio::spawn(async move {
            let mut buf = Vec::new();
            while let Some(bytes) = stdin.recv().await {
                buf.extend(bytes);
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    let (Some(method), Some(id)) = (msg["method"].as_str(), msg.get("id")) else {
                        continue;
                    };
                    log.lock().unwrap().push(method.to_string());
                    let reply = match method {
                        "initialize" => json!({"protocolVersion": 1, "agentCapabilities": {}}),
                        "session/new" => {
                            let mut r = json!({"sessionId": "s1"});
                            if let Some(modes) = &modes {
                                r["modes"] = modes.clone();
                            }
                            r
                        }
                        "session/set_mode" if set_mode_error => {
                            let err = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "no such mode"}});
                            let _ = stdout.send(format!("{err}\n").into_bytes());
                            continue;
                        }
                        "session/set_mode" => json!({}),
                        "session/prompt" => json!({"stopReason": "end_turn"}),
                        _ => json!({}),
                    };
                    let response = json!({"jsonrpc": "2.0", "id": id, "result": reply});
                    let _ = stdout.send(format!("{response}\n").into_bytes());
                }
            }
        });
        let (stuck, _) = tokio::sync::broadcast::channel(4);
        let execution = AgentExecution::new(
            crate::data::session::AgentHandle {
                id: "fake".into(),
                name: "fake".into(),
                image_tag: "fake:latest".into(),
                started_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            },
            Box::new(Done),
            Arc::new(stuck),
            None,
        );
        let session = AcpSession::from_transport(
            execution,
            transport,
            Box::new(Quiet),
            PermissionPolicy::Ask,
        )
        .with_agent_name("cline");
        (session, seen)
    }

    fn modes(current: &str) -> Value {
        json!({
            "currentModeId": current,
            "availableModes": [
                {"id": "act", "name": "Act"},
                {"id": "plan", "name": "Plan", "description": "plan first"}
            ]
        })
    }

    fn methods(seen: &Arc<StdMutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn an_advertised_mode_is_selected_and_acknowledged_before_the_first_prompt() {
        let (mut acp, seen) = session(Some(modes("act")), false);
        acp.initialize("/workspace", Some("plan")).await.unwrap();
        assert_eq!(acp.current_mode(), Some("plan"));
        acp.prompt("hello").await.unwrap();
        assert_eq!(
            methods(&seen),
            [
                "initialize",
                "session/new",
                "session/set_mode",
                "session/prompt"
            ]
        );
    }

    #[tokio::test]
    async fn an_already_current_mode_sends_no_set_mode() {
        let (mut acp, seen) = session(Some(modes("plan")), false);
        acp.initialize("/workspace", Some("plan")).await.unwrap();
        acp.prompt("hello").await.unwrap();
        assert_eq!(
            methods(&seen),
            ["initialize", "session/new", "session/prompt"]
        );
    }

    #[tokio::test]
    async fn an_unadvertised_mode_is_refused_and_no_prompt_can_follow() {
        for advertised in [
            None,
            Some(json!({"currentModeId": "act", "availableModes": [{"id": "act", "name": "Act"}]})),
        ] {
            let (mut acp, seen) = session(advertised, false);
            let err = acp
                .initialize("/workspace", Some("plan"))
                .await
                .unwrap_err();
            match &err {
                EngineError::NativePlanRouteUnsupported {
                    agent,
                    route,
                    reason,
                } => {
                    assert_eq!((agent.as_str(), *route), ("cline", "acp"));
                    assert!(
                        reason.contains("does not advertise session mode 'plan'"),
                        "{reason}"
                    );
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(err.is_native_capability_unavailable());
            let refused = acp.prompt("hello").await.unwrap_err();
            assert!(
                refused.to_string().contains("start mode not established"),
                "{refused}"
            );
            assert!(!methods(&seen).contains(&"session/prompt".to_string()));
            acp.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_set_mode_error_is_a_refusal_not_a_silent_default() {
        let (mut acp, seen) = session(Some(modes("act")), true);
        let err = acp
            .initialize("/workspace", Some("plan"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, EngineError::NativePlanRouteUnsupported { reason, .. } if reason.contains("not acknowledged")),
            "{err:?}"
        );
        let mut frontend = NoPrompts;
        let driven = acp
            .drive_with_initial_prompt(&mut frontend, Some("x".into()))
            .await;
        assert!(driven
            .unwrap_err()
            .to_string()
            .contains("start mode not established"));
        assert!(!methods(&seen).contains(&"session/prompt".to_string()));
    }

    #[tokio::test]
    async fn without_a_start_mode_nothing_changes() {
        let (mut acp, seen) = session(None, false);
        acp.initialize("/workspace", None).await.unwrap();
        assert_eq!(acp.current_mode(), None);
        acp.prompt("hello").await.unwrap();
        assert_eq!(
            methods(&seen),
            ["initialize", "session/new", "session/prompt"]
        );
    }

    #[test]
    fn a_later_mode_change_is_a_tolerated_update() {
        let update: SessionUpdate = serde_json::from_value(
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "act"}),
        )
        .unwrap();
        assert_eq!(
            crate::engine::acp::protocol::summarize_update(&update),
            "mode changed to act"
        );
    }

    struct NoPrompts;
    impl AcpFrontend for NoPrompts {
        fn render_update(&mut self, _update: SessionUpdate) {}
        fn request_permission(&mut self, _request: PermissionRequest) -> PermissionDecision {
            PermissionDecision::Cancelled
        }
        fn next_prompt(&mut self) -> Option<String> {
            None
        }
    }
}
