//! API HTTP frontend — full Axum server.
//!
//! Wire-identical to `oldsrc/commands/headless/server.rs`; the only internal
//! change is that `POST /v1/commands` dispatches through Layer 2 instead of
//! spawning a child process.

pub mod command_frontend;
pub mod event_bus;
pub mod queue_worker;
pub mod routes;
pub mod session_setup;

use crate::command::commands::api_server::ApiServeConfig;
use crate::command::error::CommandError;

/// Boot the API HTTP server and block until shutdown signal.
pub async fn serve(config: ApiServeConfig) -> Result<(), CommandError> {
    use std::sync::Arc;
    use std::time::Instant;

    use crate::data::fs::api_db::SqliteSessionStore;
    use crate::data::fs::api_paths::ApiPaths;

    let api_paths = ApiPaths::from_process_env().map_err(CommandError::Data)?;
    api_paths.ensure_root().map_err(CommandError::Data)?;

    tracing::info!(root = %api_paths.root().display(), "Opening session store");
    let store = SqliteSessionStore::open(api_paths.root()).map_err(CommandError::Data)?;

    // Startup cleanup: remove closed sessions older than 24 hours.
    if let Ok(deleted) = store.delete_closed_sessions_older_than(24) {
        for (sid, cmd_count) in &deleted {
            tracing::info!(
                session_id = %sid,
                commands = cmd_count,
                "Purging stale closed session"
            );
        }
    }

    let auth_paths = crate::data::fs::auth_paths::AuthPathResolver::from_process_env()
        .map_err(CommandError::Data)?;
    let auth_engine =
        crate::engine::auth::AuthEngine::with_paths(auth_paths.clone(), api_paths.clone());

    let auth_mode = if config.dangerously_skip_auth {
        routes::AuthMode::Disabled
    } else {
        let hash = auth_engine.read_api_key_hash()?.ok_or_else(|| {
            CommandError::Other(
                "No API key hash on disk. Run `awman api start --refresh-key` to generate one."
                    .into(),
            )
        })?;
        routes::AuthMode::Enabled {
            key_hash: hash.as_str().to_string(),
        }
    };
    let auth_enabled = matches!(auth_mode, routes::AuthMode::Enabled { .. });

    // Construct Layer 1 engines for dispatch. The container runtime is
    // resolved via the same `detect` path the CLI/TUI use so that per-user
    // `~/.awman/config.json` settings (e.g. `runtime: "apple-containers"`)
    // are honored in API mode. Per-repo `.awman/config.json` is loaded later
    // per session via `Session::open_or_workdir_fallback`.
    let global_config = crate::data::config::global::GlobalConfig::load().unwrap_or_default();
    let runtime = Arc::new(
        crate::engine::container::ContainerRuntime::detect(&global_config)
            .map_err(|e| CommandError::Other(format!("container runtime detect: {e}")))?,
    );
    tracing::info!(
        runtime = runtime.runtime_name(),
        "Container runtime resolved"
    );
    let git_engine = Arc::new(crate::engine::git::GitEngine::new());
    let overlay_engine = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
        auth_paths,
    ));
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay_engine.clone(),
        runtime.clone(),
    ));
    let auth_engine_arc = Arc::new(auth_engine);
    // Use a temporary workflow state store path; each command opens its own
    // session-scoped store via the workdir, but Engines requires one at
    // construction time.
    let workflow_state_store = Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
        api_paths.root(),
    ));

    let engines = crate::command::dispatch::Engines {
        runtime,
        git_engine,
        overlay_engine,
        auth_engine: auth_engine_arc,
        agent_engine,
        workflow_state_store,
    };

    // Restore in-memory sessions for any active sessions persisted in SQLite
    // from a previous server lifetime. This ensures session continuity across
    // server restarts.
    tracing::info!("Restoring active sessions from previous server lifetime");
    let mut restored_sessions = std::collections::HashMap::new();
    if let Ok(records) = store.list_sessions_by_status(Some("active")) {
        for rec in records {
            let workdir_path = std::path::PathBuf::from(&rec.workdir);

            // For remote sessions, the workdir IS the clone directory; if the
            // clone no longer exists on disk, close the session and skip.
            if rec.session_type == "remote" && !workdir_path.exists() {
                tracing::warn!(
                    session_id = %rec.id,
                    workdir = %rec.workdir,
                    "Remote session clone no longer exists; closing session"
                );
                let now = chrono::Utc::now().to_rfc3339();
                let _ = store.close_session_force(&rec.id, &now);
                continue;
            }

            let resolver = crate::data::session::StaticGitRootResolver::new(&workdir_path);
            match crate::data::session::Session::open_or_workdir_fallback(
                workdir_path.clone(),
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
            ) {
                Ok(mut s) => {
                    // Restore the SessionType variant for remote sessions so
                    // that worktree suppression and other type-aware code
                    // paths behave correctly. repo_url and branch aren't
                    // persisted in the schema; use empty placeholders since
                    // only the variant and cloned_path are actually consumed.
                    if rec.session_type == "remote" {
                        if let Some(cloned) = rec.cloned_path.as_deref() {
                            s.set_session_type(crate::data::session::SessionType::Remote {
                                repo_url: String::new(),
                                branch: String::new(),
                                cloned_path: std::path::PathBuf::from(cloned),
                            });
                        }
                    }
                    restored_sessions.insert(rec.id.clone(), Arc::new(tokio::sync::RwLock::new(s)));
                    tracing::info!(session_id = %rec.id, workdir = %rec.workdir, "Restored session");
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %rec.id,
                        workdir = %rec.workdir,
                        error = %e,
                        "Failed to restore session (workdir may no longer exist)"
                    );
                }
            }
        }
    }

    // Mark sessions with in-progress setup as failed (server restarted mid-setup).
    // Authoritative source is the DB's setup_status column; we also persist
    // a setup_state.json for the /status endpoint's disk fallback path so
    // that the failure reason is visible to clients.
    {
        use crate::data::session_setup_event::{
            SessionSetupError, SessionSetupState, SessionSetupStatus,
        };
        if let Ok(records) = store.list_sessions_with_in_progress_setup() {
            for rec in &records {
                tracing::warn!(
                    session_id = %rec.id,
                    previous_status = %rec.setup_status,
                    "Marking session as failed (server restarted during setup)"
                );
                let _ = store.update_setup_status(&rec.id, "failed");

                // Clean up any partial clone for remote sessions.
                if rec.session_type == "remote" {
                    if let Some(cloned) = rec.cloned_path.as_deref() {
                        let _ = std::fs::remove_dir_all(cloned);
                    }
                }

                // Update or create setup_state.json so /status reflects the
                // restart failure once the in-memory bus is gone.
                let setup_path = api_paths.session_dir(&rec.id).join("setup_state.json");
                let mut ss: SessionSetupState = std::fs::read_to_string(&setup_path)
                    .ok()
                    .and_then(|c| serde_json::from_str::<SessionSetupState>(&c).ok())
                    .unwrap_or_default();
                ss.status = SessionSetupStatus::Failed;
                ss.current_stage = Some("Server restarted during session setup".to_string());
                ss.error = Some(SessionSetupError {
                    stage: "server_restart".to_string(),
                    message: "Server restarted during session setup".to_string(),
                });
                if let Ok(json) = serde_json::to_string_pretty(&ss) {
                    let _ = std::fs::create_dir_all(api_paths.session_dir(&rec.id));
                    let _ = std::fs::write(&setup_path, json);
                }
            }
        }
    }

    let store = Arc::new(store);

    // Stale command recovery: at startup, every command still in `running`
    // is unequivocally stale — the worker that owned it died with the
    // previous server process. Use a zero-second threshold so we recover all
    // of them immediately rather than leaving recently-started ones stuck
    // for the duration of the threshold.
    match store.recover_stale_commands(0) {
        Ok(recovered) if !recovered.is_empty() => {
            tracing::info!(
                count = recovered.len(),
                "Recovered stale running commands back to queued"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to recover stale commands");
        }
        _ => {}
    }

    let sessions = Arc::new(tokio::sync::Mutex::new(restored_sessions));
    let event_buses = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    let state = Arc::new(routes::AppState {
        store: Arc::clone(&store),
        paths: api_paths,
        workdirs: config.workdirs,
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode,
        engines: engines.clone(),
        sessions: Arc::clone(&sessions),
        event_buses: Arc::clone(&event_buses),
        setup_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });

    // Spawn queue workers.
    let worker_count = global_config.workers();
    if worker_count == 0 {
        tracing::warn!("workers config is 0 — no queue workers will run; commands will be enqueued but never processed");
    }
    for i in 0..worker_count {
        let worker = queue_worker::QueueWorker::new(
            format!("worker-{i}-{}", uuid::Uuid::new_v4()),
            Arc::clone(&store),
            engines.clone(),
            Arc::clone(&sessions),
            Arc::clone(&event_buses),
            state.paths.clone(),
        );
        tokio::spawn(worker.run());
        tracing::debug!(worker_index = i, "Spawned queue worker");
    }
    tracing::info!(count = worker_count, "Queue workers started");

    let app = routes::build_router(state.clone());
    let addr = std::net::SocketAddr::from((config.bind_ip, config.port));

    let scheme = if config.tls_material.is_some() {
        "https"
    } else {
        "http"
    };
    tracing::info!(
        port = config.port,
        bind_ip = %config.bind_ip,
        tls = config.tls_material.is_some(),
        auth = auth_enabled,
        "awman API mode starting"
    );
    // Pre-bind announce so the user sees the endpoint BEFORE the server
    // blocks on the accept loop. The matching post-bind "listening" log
    // below previously sat after `.await` and never fired in the success
    // path (the future only resolves on shutdown).
    tracing::info!(
        endpoint = %format!("{scheme}://{}:{}", config.bind_ip, config.port),
        "awman API mode listening — binding HTTP listener"
    );

    // Spawn the shutdown signal as a background task — we trigger
    // axum-server's graceful shutdown handle when it fires.
    let server_handle = axum_server::Handle::new();
    let shutdown_handle = server_handle.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("Failed to install SIGTERM handler: {e}");
                        return;
                    }
                };
            tokio::select! {
                _ = ctrl_c => { tracing::info!("Received SIGINT, shutting down"); }
                _ = sigterm.recv() => { tracing::info!("Received SIGTERM, shutting down"); }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("Received SIGINT, shutting down");
        }
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(30)));
    });

    let serve_result = if let Some(tls) = config.tls_material.clone() {
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
            tls.cert_pem.into_bytes(),
            tls.key_pem.into_bytes(),
        )
        .await
        .map_err(|e| CommandError::Other(format!("TLS setup: {e}")))?;
        axum_server::bind_rustls(addr, rustls_config)
            .handle(server_handle.clone())
            .serve(app.into_make_service())
            .await
    } else {
        axum_server::bind(addr)
            .handle(server_handle.clone())
            .serve(app.into_make_service())
            .await
    };

    serve_result.map_err(|e| {
        if let Some(io) = e.raw_os_error() {
            // Linux EADDRINUSE = 98, macOS = 48, Windows = 10048
            if matches!(io, 98 | 48 | 10048) {
                return CommandError::Other(format!(
                    "Port {} is already in use. Use --port to choose a different port.",
                    config.port
                ));
            }
        }
        if e.to_string()
            .to_lowercase()
            .contains("address already in use")
        {
            return CommandError::Other(format!(
                "Port {} is already in use. Use --port to choose a different port.",
                config.port
            ));
        }
        CommandError::Other(format!("Server error: {e}"))
    })?;

    tracing::info!(
        port = config.port,
        "awman API mode stopped accepting new connections"
    );

    // Grace period for running commands (30s).
    const GRACE_SECS: u64 = 30;
    let handles: Vec<_> = state.task_handles.lock().await.drain(..).collect();
    if !handles.is_empty() {
        tracing::info!(
            count = handles.len(),
            grace_seconds = GRACE_SECS,
            "Waiting for running commands to finish"
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(GRACE_SECS);
        for handle in handles {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                handle.abort();
            } else {
                let _ = tokio::time::timeout(remaining, handle).await;
            }
        }
    }

    tracing::info!("awman API mode stopped");
    Ok(())
}
