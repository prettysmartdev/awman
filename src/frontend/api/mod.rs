//! API HTTP frontend — router, listener, signal handling, and serving only.

pub mod command_frontend;
pub mod routes;
pub mod serve;

use std::sync::Arc;

use crate::command::commands::api_server::queue_worker::ApiCommandFrontendFactory;
use crate::command::commands::api_server::{ApiServerRuntime, AuthMode};
use crate::command::error::CommandError;

/// Layer 3 construction of the concrete API presentation adapter.
#[derive(Clone, Copy, Default)]
pub struct ApiFrontendFactory;

impl ApiCommandFrontendFactory for ApiFrontendFactory {
    type Frontend = command_frontend::ApiDispatchFrontend;

    fn frontend_for(
        &self,
        command: &crate::data::fs::api_db::CommandRecord,
        event_bus: crate::command::commands::api_server::event_bus::EventBusSender,
    ) -> Self::Frontend {
        let args: Vec<String> = serde_json::from_str(&command.args).unwrap_or_default();
        command_frontend::ApiDispatchFrontend::new(&command.subcommand, &args, event_bus)
    }
}

/// Serve a fully bootstrapped API runtime until the shutdown signal arrives.
pub async fn serve(runtime: ApiServerRuntime) -> Result<(), CommandError> {
    let auth_enabled = matches!(runtime.auth_mode, AuthMode::Enabled { .. });
    runtime.spawn_workers(ApiFrontendFactory);
    let addr = runtime.bind_addr();
    let tls = runtime.tls_material();
    let state = Arc::new(routes::AppState {
        store: runtime.store,
        paths: runtime.paths,
        workdirs: runtime.workdirs,
        started_at: runtime.started_at,
        task_handles: runtime.task_handles,
        auth_mode: runtime.auth_mode,
        engines: runtime.engines,
        sessions: runtime.sessions,
        event_buses: runtime.event_buses,
        setup_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let app = routes::build_router(Arc::clone(&state));
    tracing::info!(addr = %addr, tls = tls.is_some(), auth = auth_enabled, "awman API mode starting");
    serve::serve_router(
        app,
        serve::ServeOptions {
            addr,
            tls,
            shutdown_grace: std::time::Duration::from_secs(30),
        },
    )
    .await?;

    const GRACE_SECS: u64 = 30;
    let handles: Vec<_> = state.task_handles.lock().await.drain(..).collect();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(GRACE_SECS);
    for handle in handles {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            handle.abort();
        } else {
            let _ = tokio::time::timeout(remaining, handle).await;
        }
    }
    tracing::info!("awman API mode stopped");
    Ok(())
}
