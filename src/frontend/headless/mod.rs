//! Headless HTTP frontend — full Axum server.
//!
//! Wire-identical to `oldsrc/commands/headless/server.rs`; the only internal
//! change is that `POST /v1/commands` dispatches through Layer 2 instead of
//! spawning a child process.

pub mod command_frontend;
pub mod routes;

use crate::command::commands::headless::HeadlessServeConfig;
use crate::command::error::CommandError;

/// Boot the headless HTTP server and block until shutdown signal.
pub async fn serve(config: HeadlessServeConfig) -> Result<(), CommandError> {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Instant;

    use crate::data::fs::headless_db::SqliteSessionStore;
    use crate::data::fs::headless_paths::HeadlessPaths;

    let headless_paths = HeadlessPaths::from_process_env().map_err(CommandError::Data)?;
    headless_paths.ensure_root().map_err(CommandError::Data)?;

    let store = SqliteSessionStore::open(headless_paths.root()).map_err(CommandError::Data)?;

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
        crate::engine::auth::AuthEngine::with_paths(auth_paths.clone(), headless_paths.clone());

    let auth_mode = if config.dangerously_skip_auth {
        routes::AuthMode::Disabled
    } else {
        let hash = auth_engine.read_api_key_hash()?.ok_or_else(|| {
            CommandError::Other(
                "No API key hash on disk. Run `amux auth --refresh-key` first.".into(),
            )
        })?;
        routes::AuthMode::Enabled {
            key_hash: hash.as_str().to_string(),
        }
    };

    // Construct Layer 1 engines for dispatch.
    let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
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
        headless_paths.root(),
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
    let mut restored_sessions = std::collections::HashMap::new();
    if let Ok(records) = store.list_sessions_by_status(Some("active")) {
        for rec in records {
            let workdir_path = std::path::PathBuf::from(&rec.workdir);
            let resolver = crate::data::session::StaticGitRootResolver::new(&workdir_path);
            match crate::data::session::Session::open_or_workdir_fallback(
                workdir_path,
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
            ) {
                Ok(s) => {
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

    let state = Arc::new(routes::AppState {
        store,
        paths: headless_paths,
        workdirs: config.workdirs,
        started_at: Instant::now(),
        busy_sessions: tokio::sync::Mutex::new(HashSet::new()),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode,
        engines,
        sessions: tokio::sync::Mutex::new(restored_sessions),
    });

    let app = routes::build_router(state.clone());
    let addr = std::net::SocketAddr::from((config.bind_ip, config.port));

    tracing::info!(
        port = config.port,
        bind_ip = %config.bind_ip,
        tls = config.tls_material.is_some(),
        "Headless server starting"
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

    tracing::info!(port = config.port, "Headless server listening");

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

    tracing::info!("Headless server stopped");
    Ok(())
}
