//! `RemoteClient` — the typed HTTP client for an awman API server.
//!
//! Layer 1 (WI 0114 F-28): the routes, the request and response bodies and
//! the API-key resolution rule are the engine's typed API for talking to a
//! remote awman. Layer 2 keeps what depends on Layer 2 — `RemoteCommand`,
//! and the workflow-poller family in `command::commands::remote_client`,
//! whose `SquadTaskWorkflowSource` is backed by the Layer 2 `TaskGateway`.

use std::time::Duration;

use serde::Deserialize;

use crate::data::fs::api_labels::Labels;
use crate::data::session_setup_event::SessionSetupState;
use crate::engine::auth::ApiKey;
use crate::engine::error::EngineError;
use crate::engine::remote::http_core::{HttpCore, HttpResponse};

/// Typed HTTP client for talking to a remote awman API server. A thin façade
/// over one [`HttpCore`]: `RemoteClient` owns only the route-specific methods,
/// the generic transport lives in the core.
pub struct RemoteClient {
    pub(super) core: HttpCore,
}

/// Response for the low-level verb helpers. Kept as a type alias so external
/// callers referencing `remote_client::RemoteResponse` (e.g. `get_job`'s return
/// type) do not change.
pub type RemoteResponse = HttpResponse;

/// The terminal-aware status vocabulary used by remote workflow polling.
/// Parsing the wire strings here keeps status semantics out of frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    #[serde(alias = "pending")]
    Queued,
    Running,
    Done,
    Error,
    Cancelled,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Error | Self::Cancelled)
    }
}
/// Request body for `POST /v1/worker/sessions`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StartSessionRequest {
    pub session_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_refs: Vec<String>,
    #[serde(default)]
    pub push: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Labels>,
}

/// Command accepted by `POST /v1/worker/commands` or nested in `POST /v1/worker/runs`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartCommandRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction: Option<crate::data::config::daemon::Interaction>,
    pub subcommand: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub labels: Labels,
}

/// One submission; setup readiness and optional session cleanup are server-owned.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartRunRequest {
    pub session: StartSessionRequest,
    pub command: StartCommandRequest,
    #[serde(default)]
    pub close_on_finish: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartRunResponse {
    pub session_id: String,
    pub command_id: String,
}

/// Both terminal (200) and stopping (202) cancel responses identify the command.
#[derive(Debug, Clone, Deserialize)]
pub struct CancelCommandResponse {
    #[serde(alias = "id")]
    pub command_id: String,
    pub session_id: String,
    pub status: JobStatus,
    #[serde(default)]
    pub cancel_requested_at: Option<String>,
    #[serde(default)]
    pub labels: Labels,
}

#[derive(Debug, Clone, Default)]
pub struct CommandListQuery {
    pub session_id: Option<String>,
    pub status: Option<JobStatus>,
    /// Repeatable ANDed selectors (`key=value` or `key`).
    pub labels: Vec<String>,
    pub limit: Option<i64>,
}

/// `StartSession` response body.
#[derive(Debug, Clone, Deserialize)]
pub struct StartSessionResponse {
    #[serde(rename = "id")]
    pub session_id: String,
}

/// Exec routing argument — either a workflow path or a one-shot prompt.
/// A workflow path may be omitted only for `exec workflow --dynamic`.
#[derive(Debug, Clone)]
pub enum ExecArg {
    Workflow(Option<String>),
    /// `None` when the prompt comes from a forwarded `--work-item` alone.
    Prompt(Option<String>),
}

impl ExecArg {
    /// The `POST /v1/worker/commands` body for this exec with `flags` (argv tokens).
    /// Flags come first and the positional follows a `--`: `exec prompt`'s
    /// prompt is a greedy trailing argument that would otherwise swallow
    /// every flag after it, and a prompt or path starting with `-` must never
    /// be read as a flag.
    pub fn into_request(self, flags: &[String]) -> StartCommandRequest {
        let (subcommand, positional) = match self {
            ExecArg::Workflow(path) => ("exec workflow", path),
            ExecArg::Prompt(text) => ("exec prompt", text),
        };
        let interaction = flags
            .iter()
            .any(|s| s == "--headless" || s == "-n")
            .then_some(crate::data::config::daemon::Interaction::Headless);
        let mut args: Vec<String> = flags
            .iter()
            .filter(|s| s.as_str() != "--headless" && s.as_str() != "-n")
            .cloned()
            .collect();
        if let Some(positional) = positional {
            args.push("--".into());
            args.push(positional);
        }
        StartCommandRequest {
            interaction,
            subcommand: subcommand.into(),
            args,
            labels: Labels::empty(),
        }
    }
}

/// Response for `POST /v1/worker/commands`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExecJobResponse {
    #[serde(rename = "id")]
    pub command_id: String,
    #[serde(default)]
    pub flags_applied: serde_json::Value,
}

/// Projection of `GET /v1/worker/sessions/{id}` (its `setup`, `setup_status`, `setup_error`) as a `SessionSetupState`
/// with the session id echoed back.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionSetupStatusResponse {
    pub session_id: String,
    #[serde(flatten)]
    pub state: SessionSetupState,
}

impl RemoteClient {
    /// The API version prefix. Hardcoded here (rather than at every call site)
    /// and threaded into the shared [`HttpCore`].
    const PREFIX: &'static str = "v1";
    pub fn for_target(
        target: &crate::data::config::remote::ResolvedRemoteTarget,
    ) -> Result<Self, EngineError> {
        let key = target.api_key.as_deref().map(ApiKey::from_string);
        Self::new(&target.url, key.as_ref())
    }

    pub const CONNECT_TIMEOUT: Duration = HttpCore::CONNECT_TIMEOUT;
    pub const READ_TIMEOUT: Duration = HttpCore::READ_TIMEOUT;

    pub fn new(base_url: &str, api_key: Option<&ApiKey>) -> Result<Self, EngineError> {
        Self::new_with_pinned_cert(base_url, api_key, None)
    }

    /// Construct a client that additionally trusts a specific PEM-encoded
    /// certificate. Used when talking to a loopback awman API server with
    /// a self-signed cert: the cert PEM is loaded from the local `tls/`
    /// directory and added as a trusted root, effectively pinning by identity.
    /// For non-loopback targets, the caller MUST NOT pass `pinned_cert_pem` —
    /// standard webpki verification stays in force.
    pub fn new_with_pinned_cert(
        base_url: &str,
        api_key: Option<&ApiKey>,
        pinned_cert_pem: Option<&str>,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            core: HttpCore::new_with_pinned_cert(base_url, Self::PREFIX, api_key, pinned_cert_pem)?,
        })
    }

    /// Returns `true` when `addr` resolves to a loopback host (`127.0.0.1`,
    /// `::1`, `localhost`). Used to decide whether the locally-stored
    /// self-signed cert should be trusted.
    pub fn is_loopback_addr(addr: &str) -> bool {
        HttpCore::is_loopback_addr(addr)
    }

    // ─── Typed methods (preferred public surface) ────────────────────────────

    /// `POST /v1/worker/sessions` — request session creation. Returns the new session
    /// id; setup runs asynchronously and the session is not ready for jobs
    /// until `GET /v1/worker/sessions/{id}` reports `setup_status: "ready"`.
    pub async fn start_session(
        &self,
        req: &StartSessionRequest,
    ) -> Result<StartSessionResponse, EngineError> {
        self.start_session_with_idempotency(req, None).await
    }

    pub async fn start_session_with_idempotency(
        &self,
        req: &StartSessionRequest,
        idempotency_key: Option<&str>,
    ) -> Result<StartSessionResponse, EngineError> {
        let url = self.core.url(&["worker", "sessions"]);
        let mut request = self.core.http().post(&url).json(req);
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let resp = request.send().await.map_err(Self::map_reqwest_error)?;
        let status = resp.status().as_u16();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(Self::map_reqwest_error)?;
        if status >= 400 {
            return Err(EngineError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        serde_json::from_value::<StartSessionResponse>(body)
            .map_err(|e| EngineError::Other(format!("invalid start-session response: {e}")))
    }

    /// `DELETE /v1/worker/sessions/{id}` — kill a session.
    pub async fn kill_session(&self, session_id: &str) -> Result<(), EngineError> {
        let _ = self.delete(&["worker", "sessions", session_id]).await?;
        Ok(())
    }

    /// `GET /v1/worker/sessions/{id}` — fetch the deserialized setup state.
    pub async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<SessionSetupStatusResponse, EngineError> {
        let resp = self.get(&["worker", "sessions", session_id]).await?;
        let mut state: SessionSetupState = resp
            .body
            .get("setup")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| EngineError::RemoteTransport(e.to_string()))?
            .unwrap_or_default();
        state.status = serde_json::from_value(resp.body["setup_status"].clone())
            .map_err(|e| EngineError::RemoteTransport(e.to_string()))?;
        if let Some(error) = resp.body.get("setup_error") {
            state.error = serde_json::from_value(error.clone())
                .map_err(|e| EngineError::RemoteTransport(e.to_string()))?;
        }
        Ok(SessionSetupStatusResponse {
            session_id: session_id.into(),
            state,
        })
    }

    /// Submit an exec-prompt or exec-workflow job. Returns the command id.
    pub async fn exec_job(
        &self,
        session_id: &str,
        exec: ExecArg,
        extra_args: &[String],
    ) -> Result<ExecJobResponse, EngineError> {
        self.exec_job_with_idempotency(session_id, exec, extra_args, None)
            .await
    }

    pub async fn exec_job_with_idempotency(
        &self,
        session_id: &str,
        exec: ExecArg,
        extra_args: &[String],
        idempotency_key: Option<&str>,
    ) -> Result<ExecJobResponse, EngineError> {
        self.submit_command(session_id, &exec.into_request(extra_args), idempotency_key)
            .await
    }

    /// Submit a command with optional labels and idempotency, to a ready session.
    pub async fn submit_command(
        &self,
        session_id: &str,
        request: &StartCommandRequest,
        idempotency_key: Option<&str>,
    ) -> Result<ExecJobResponse, EngineError> {
        let mut headers = vec![("x-awman-session", session_id)];
        if let Some(key) = idempotency_key {
            headers.push(("Idempotency-Key", key));
        }

        let resp = self
            .send_command_with_headers(
                &["worker", "commands"],
                &[
                    ("subcommand", serde_json::json!(request.subcommand)),
                    ("interaction", serde_json::json!(request.interaction)),
                    ("args", serde_json::json!(request.args)),
                    ("labels", serde_json::json!(request.labels)),
                ],
                &headers,
            )
            .await?;

        serde_json::from_value::<ExecJobResponse>(resp.body)
            .map_err(|e| EngineError::Other(format!("invalid exec response: {e}")))
    }

    /// `POST /v1/worker/runs` accepts the command immediately; no readiness polling.
    pub async fn start_run(
        &self,
        request: &StartRunRequest,
        idempotency_key: Option<&str>,
    ) -> Result<StartRunResponse, EngineError> {
        let headers = idempotency_key
            .map(|key| vec![("Idempotency-Key", key)])
            .unwrap_or_default();
        let response = self
            .send_command_with_headers(
                &["worker", "runs"],
                &[
                    ("session", serde_json::json!(request.session)),
                    ("command", serde_json::json!(request.command)),
                    (
                        "close_on_finish",
                        serde_json::json!(request.close_on_finish),
                    ),
                ],
                &headers,
            )
            .await?;
        serde_json::from_value(response.body)
            .map_err(|e| EngineError::Other(format!("invalid start-run response: {e}")))
    }

    pub async fn cancel_command(&self, id: &str) -> Result<CancelCommandResponse, EngineError> {
        let response = self
            .send_command_with_headers(&["worker", "commands", id, "cancel"], &[], &[])
            .await?;
        serde_json::from_value(response.body)
            .map_err(|e| EngineError::Other(format!("invalid cancel response: {e}")))
    }

    pub async fn list_commands(
        &self,
        query: &CommandListQuery,
    ) -> Result<RemoteResponse, EngineError> {
        let mut pairs: Vec<(&str, String)> = query
            .labels
            .iter()
            .map(|label| ("label", label.clone()))
            .collect();
        if let Some(id) = &query.session_id {
            pairs.push(("session_id", id.clone()));
        }
        if let Some(status) = query.status {
            let value =
                serde_json::to_value(status).map_err(|e| EngineError::Other(e.to_string()))?;
            pairs.push(("status", value.as_str().unwrap().to_owned()));
        }
        if let Some(limit) = query.limit {
            pairs.push(("limit", limit.to_string()));
        }
        let mut url = reqwest::Url::parse(&self.core.url(&["worker", "commands"]))
            .map_err(|e| EngineError::RemoteTransport(e.to_string()))?;
        url.query_pairs_mut().extend_pairs(pairs);
        let response = self
            .core
            .http()
            .get(url)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let status = response.status().as_u16();
        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(Self::map_reqwest_error)?;
        if status >= 400 {
            return Err(EngineError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(RemoteResponse { status, body })
    }

    /// `GET /v1/worker/commands/{id}` — fetch a job's record.
    pub async fn get_command(&self, command_id: &str) -> Result<RemoteResponse, EngineError> {
        self.get(&["worker", "commands", command_id]).await
    }

    /// `GET /v1/worker/commands/{id}` — the typed job status used by
    /// remote workflow polling.
    pub async fn job_status(&self, id: &str) -> Result<JobStatus, EngineError> {
        let response = self.get_command(id).await?;
        let status = response.body["status"].as_str().ok_or_else(|| {
            EngineError::RemoteTransport("remote job status response has no status".into())
        })?;
        match status {
            "queued" | "pending" => Ok(JobStatus::Queued),
            "running" => Ok(JobStatus::Running),
            "done" => Ok(JobStatus::Done),
            "error" => Ok(JobStatus::Error),
            "cancelled" => Ok(JobStatus::Cancelled),
            other => Err(EngineError::RemoteTransport(format!(
                "unknown remote job status: {other}"
            ))),
        }
    }

    /// `GET /v1/worker/commands/{id}/workflow` — fetch the workflow state JSON for a job.
    /// Returns `None` on HTTP 404 (job is a prompt job or pending).
    pub async fn get_workflow_state(
        &self,
        command_id: &str,
    ) -> Result<Option<serde_json::Value>, EngineError> {
        match self
            .get(&["worker", "commands", command_id, "workflow"])
            .await
        {
            Ok(resp) => Ok(Some(resp.body)),
            Err(EngineError::RemoteHttpStatus { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
    // ─── Generic low-level helpers (crate-private) ───────────────────────────

    /// Like `send_command` but also attaches request headers — used to set
    /// `x-awman-session` on `POST /v1/worker/commands` (the server reads the session
    /// from the header, not the body).
    pub(crate) async fn send_command_with_headers(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
        headers: &[(&str, &str)],
    ) -> Result<RemoteResponse, EngineError> {
        self.core.post_command(path, flags, headers).await
    }

    pub(crate) async fn get(&self, path: &[&str]) -> Result<RemoteResponse, EngineError> {
        self.core.get(path).await
    }

    pub(crate) async fn delete(&self, path: &[&str]) -> Result<RemoteResponse, EngineError> {
        self.core.delete(path).await
    }

    pub fn map_reqwest_error(e: reqwest::Error) -> EngineError {
        HttpCore::map_reqwest_error(e)
    }
}
