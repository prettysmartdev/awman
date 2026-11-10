//! The push side of the worker feature, in process (WI 0143).
//!
//! Drives the real Axum router (`/v1/worker/commands/{id}/events` SSE,
//! `/v1/worker/commands/{id}`) over loopback with the parts the queue worker
//! uses: a claimed store row, `EventBus::for_attempt`, the reliable log
//! consumer (`CommandLogWriter`). Modelled on
//! `tests/api_parity/command_log_stream.rs`; no Docker needed.
//!
//! The log consumer can *tee* every persisted event into a channel, which
//! is how a pulled job's ordered log reaches the WSP event forwarder: the
//! same ordered log feeds push SSE and `POST /v1/leases/{id}/events`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::Engines;
use awman::data::command_status::CommandStatus;
use awman::data::execution_event::{EventPayload, ExecutionEvent};
use awman::data::fs::api_command_log::CommandLogWriter;
use awman::data::fs::api_db::{NewSessionRow, SqliteSessionStore};
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::session::SessionKind;
use awman::data::session_setup_event::SessionSetupStatus;
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::routes::{build_router, AppState};
use tokio::sync::mpsc;

pub const SESSION: &str = "ses_f5b338d6bca36d47ee04d93d08c57861";

pub fn make_state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().expect("ensure_root");
    let store = Arc::new(SqliteSessionStore::open(paths.root()).expect("open sqlite"));
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(paths.root())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    let lifecycle_store = awman::data::CommandLifecycleStore::open(Arc::clone(&store)).unwrap();
    let setup_cancels = Arc::new(awman::command::commands::api_server::TokenRegistry::new());
    let canceller = awman::command::commands::api_server::CommandCanceller::new(
        Arc::clone(&store),
        lifecycle_store.clone(),
        Arc::new(awman::command::commands::api_server::TokenRegistry::new()),
        Arc::clone(&setup_cancels),
    );
    store
        .insert_session_full(NewSessionRow {
            id: SESSION,
            workdir: "/work",
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    Arc::new(AppState {
        live_config: std::sync::RwLock::new(None),
        settings_tx: None,
        lifecycle_store,
        setup_cancels,
        canceller,
        store,
        paths,
        workdirs: vec![],
        workdir_roots: Vec::new(),
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        engines,
        sessions: Arc::new(awman::data::session_manager::SessionManager::in_memory(
            awman::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

pub async fn serve(state: Arc<AppState>) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

pub fn enqueue(state: &AppState, id: &str) {
    enqueue_in(state, SESSION, id);
}

/// Add another ready session (commands of one session run one at a time).
pub fn add_session(state: &AppState, session: &str) {
    state
        .store
        .insert_session_full(NewSessionRow {
            id: session,
            workdir: "/work2",
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
}

pub fn enqueue_in(state: &AppState, session: &str, id: &str) {
    state.paths.prepare_command_dir(session, id).unwrap();
    let log = state.paths.command_log_path(session, id);
    state
        .store
        .enqueue_command(id, session, "exec prompt", "[]", &log.to_string_lossy())
        .unwrap();
}

/// One execution of a command: its bus and the log consumer task.
pub struct Attempt {
    pub bus: Arc<EventBus>,
    writer: tokio::task::JoinHandle<()>,
}

/// Claim `id`'s row and start its attempt. Every event the log persists is
/// also sent to `tee` (when given), in log order, after it was written.
pub async fn start_attempt(
    state: &Arc<AppState>,
    id: &str,
    tee: Option<mpsc::Sender<ExecutionEvent>>,
) -> Attempt {
    let record = state.store.claim_next_command("w").unwrap().unwrap();
    assert_eq!(record.id, id);
    let bus = Arc::new(EventBus::for_attempt(4096, record.attempt));
    let mut log_rx = bus.subscribe_log();
    let mut writer = CommandLogWriter::open_append(&state.paths, &record.session_id, id)
        .await
        .unwrap();
    let writer = tokio::spawn(async move {
        while let Some(event) = log_rx.recv().await {
            let done = matches!(event.payload, EventPayload::Done);
            writer.write_event(&event).await;
            writer.flush().await;
            if let Some(tee) = &tee {
                let _ = tee.send(event).await;
            }
            if done {
                break;
            }
        }
    });
    state
        .event_buses
        .lock()
        .await
        .insert(id.to_string(), Arc::clone(&bus));
    Attempt { bus, writer }
}

impl Attempt {
    pub fn emit(&self, payload: EventPayload) {
        self.bus.sender().emit(payload);
    }

    /// Emit `Done`, wait for the log to flush, then persist the terminal row
    /// with `result_json`.
    pub async fn finish(self, state: &AppState, id: &str, result_json: Option<&str>) {
        self.bus.sender().emit(EventPayload::Done);
        self.writer.await.unwrap();
        state
            .store
            .complete_command(id, CommandStatus::Done, Some(0), result_json)
            .unwrap();
        state.event_buses.lock().await.remove(id);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: Option<String>,
}

impl Frame {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(self.data.as_deref().expect("frame has data")).unwrap()
    }
}

/// A minimal SSE reader for `GET /v1/worker/commands/{id}/events`.
pub struct Sse {
    response: reqwest::Response,
    buf: String,
}

impl Sse {
    pub async fn open(base: &str, command: &str) -> Sse {
        let response = reqwest::Client::new()
            .get(format!("{base}/v1/worker/commands/{command}/events"))
            .send()
            .await
            .expect("GET events");
        assert!(response.status().is_success(), "{}", response.status());
        Sse {
            response,
            buf: String::new(),
        }
    }

    pub async fn next(&mut self) -> Option<Frame> {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let mut frame = Frame::default();
                for line in block.lines() {
                    if let Some(rest) = line.strip_prefix("id:") {
                        frame.id = Some(rest.trim().to_string());
                    } else if let Some(rest) = line.strip_prefix("event:") {
                        frame.event = Some(rest.trim().to_string());
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        frame.data = Some(rest.trim_start().to_string());
                    }
                }
                if frame.data.is_none() {
                    continue; // keep-alive comment
                }
                return Some(frame);
            }
            match self.response.chunk().await {
                Ok(Some(bytes)) => self
                    .buf
                    .push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n")),
                _ => return None,
            }
        }
    }

    /// Every frame up to and including `done`.
    pub async fn until_done(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) =
            tokio::time::timeout(std::time::Duration::from_secs(20), self.next())
                .await
                .expect("SSE frame")
        {
            let done = frame.event.as_deref() == Some("done");
            frames.push(frame);
            if done {
                break;
            }
        }
        frames
    }
}
