//! ACP session lifecycle and portable frontend driver.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, Mutex};

/// Deadline for the one-shot ACP handshake requests (`initialize`,
/// `session/new`). These must complete quickly; a peer that answers
/// `initialize` and then never answers `session/new` would otherwise hang awman
/// forever (stdout stays open, so the connection-close path never fires).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

use crate::data::message::UserMessageSink;
use crate::engine::acp::client::{AcpClient, AcpTransport, IncomingRequest};
use crate::engine::acp::frontend::AcpFrontend;
use crate::engine::acp::protocol::{
    ContentBlock, InitializeRequest, InitializeResponse, JsonRpcId, NewSessionRequest,
    NewSessionResponse, PermissionDecision, PermissionRequest, PromptRequest, PromptResponse,
    SessionUpdate,
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
    permissions_rx: mpsc::UnboundedReceiver<IncomingRequest>,
    pending_permissions: Mutex<HashSet<JsonRpcId>>,
    updates_rx: mpsc::UnboundedReceiver<SessionUpdate>,
    permissions: PermissionPolicy,
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
        Self {
            execution,
            client: Arc::new(client),
            session_id: Mutex::new(None),
            permissions_rx,
            pending_permissions: Mutex::new(HashSet::new()),
            updates_rx,
            permissions,
        }
    }

    /// Negotiate ACP v1 then create a session rooted at the supplied absolute
    /// *container* working directory.
    pub async fn initialize(&mut self, cwd: impl AsRef<Path>) -> Result<String, EngineError> {
        let cwd = cwd.as_ref();
        if !cwd.is_absolute() {
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
    pub async fn shutdown(&mut self) -> Result<AgentExitInfo, EngineError> {
        let _ = self.cancel().await;
        let _ = self.execution.cancel();
        self.execution.wait().await
    }

    pub async fn prompt(&mut self, text: impl Into<String>) -> Result<PromptResponse, EngineError> {
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
            self.client.notify(
                "session/cancel",
                serde_json::json!({"sessionId": session_id}),
            )?;
        }
        let ids: Vec<_> = self.pending_permissions.lock().await.drain().collect();
        for id in ids {
            self.client
                .respond_permission(id, PermissionDecision::Cancelled)?;
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
        self.client.respond_permission(request_id, decision)
    }

    pub fn execution(&self) -> &AgentExecution {
        &self.execution
    }
    pub async fn wait(&mut self) -> Result<AgentExitInfo, EngineError> {
        self.execution.wait().await
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
    pub async fn drive_with_initial_prompt(
        &mut self,
        frontend: &mut dyn AcpFrontend,
        initial: Option<String>,
    ) -> Result<AgentExitInfo, EngineError> {
        let mut next = initial;
        loop {
            let prompt = match next.take() {
                Some(prompt) => prompt,
                None => match frontend.next_prompt() {
                    Some(prompt) => prompt,
                    None => {
                        self.cancel().await?;
                        // `None` means the whole awman session ends, so also use
                        // the normal execution cancellation path instead of
                        // waiting indefinitely for a cooperative agent to exit.
                        self.execution.cancel()?;
                        return self.execution.wait().await;
                    }
                },
            };
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
                    result = &mut turn => {
                        // The reader enqueues a turn's `session/update`s before
                        // the prompt response that ends the turn, so anything
                        // still queued here belongs to this turn — drain and
                        // render it before finishing (otherwise the tail of a
                        // turn, or the whole of a fast headless turn, is lost).
                        while let Ok(update) = self.updates_rx.try_recv() {
                            frontend.render_update(update);
                        }
                        if let Err(e) = result {
                            // The turn RPC failed (typically the agent dropped
                            // the connection mid-turn). Reap the child so it is
                            // never leaked, then surface the error.
                            let _ = self.execution.cancel();
                            let _ = self.execution.wait().await;
                            return Err(e);
                        }
                        break;
                    }
                    update = self.updates_rx.recv() => match update {
                        Some(update) => frontend.render_update(update),
                        // Update channel closed => the ACP reader task ended
                        // because stdout closed: the connection dropped. Cancel
                        // and reap the execution and return its AgentExitInfo
                        // (the edge case requires a done/error transition, not a
                        // hang) — cancel covers a peer that closes stdout but
                        // keeps its process alive.
                        None => {
                            let _ = self.execution.cancel();
                            return self.execution.wait().await;
                        }
                    },
                    request = self.permissions_rx.recv() => match request {
                        Some(IncomingRequest::Permission(request)) => self.handle_permission(frontend, request).await?,
                        // Incoming channel closed => connection dropped; reap.
                        None => {
                            let _ = self.execution.cancel();
                            return self.execution.wait().await;
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
