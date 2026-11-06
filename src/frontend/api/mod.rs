//! API HTTP frontend — router, listener, signal handling, and serving only.

pub mod command_frontend;
pub mod error;
pub mod local;
pub mod outputs;
pub mod routes;
pub mod serve;
pub mod workflow_view;

use std::sync::Arc;

use crate::command::commands::api_server::queue_worker::ApiCommandFrontendFactory;
use crate::command::commands::api_server::{ApiServerRuntime, AuthMode};
use crate::command::error::CommandError;
use crate::data::fs::api_command_log::CommandStateWriter;

/// Layer 3 construction of the concrete API presentation adapter.
#[derive(Clone, Copy, Default)]
pub struct ApiFrontendFactory;

impl ApiCommandFrontendFactory for ApiFrontendFactory {
    type Frontend = command_frontend::ApiDispatchFrontend;

    fn frontend_for(
        &self,
        command: &crate::data::fs::api_db::CommandRecord,
        event_bus: crate::command::commands::api_server::event_bus::EventBusSender,
        state_writer: CommandStateWriter,
    ) -> Self::Frontend {
        let args: Vec<String> = serde_json::from_str(&command.args).unwrap_or_default();
        command_frontend::ApiDispatchFrontend::new(&command.subcommand, &args, event_bus)
            .with_interaction(
                if args
                    .iter()
                    .take_while(|s| s.as_str() != "--")
                    .any(|s| s == "--headless=false")
                {
                    crate::data::config::daemon::Interaction::Interactive
                } else {
                    crate::data::config::daemon::Interaction::Headless
                },
            )
            .with_state_writer(state_writer)
            .deferring_done()
    }
}

/// Serve a fully bootstrapped API runtime until the shutdown signal arrives.
pub async fn serve(runtime: ApiServerRuntime) -> Result<(), CommandError> {
    let auth_enabled = matches!(runtime.auth_mode, AuthMode::Enabled { .. });
    let settings_tx = runtime.spawn_workers(ApiFrontendFactory);
    let addr = runtime.bind_addr();
    let tls = runtime.tls_material();
    let canceller = runtime.canceller();
    let config = runtime.config.clone();
    let state = Arc::new(routes::AppState {
        live_config: std::sync::RwLock::new(Some(config.clone())),
        settings_tx: Some(settings_tx),
        lifecycle_store: runtime.lifecycle_store,
        setup_cancels: runtime.setup_cancels,
        canceller,
        store: runtime.store,
        paths: runtime.paths,
        workdirs: runtime.workdirs,
        workdir_roots: runtime.workdir_roots,
        started_at: runtime.started_at,
        task_handles: runtime.task_handles,
        auth_mode: runtime.auth_mode,
        engines: runtime.engines,
        sessions: runtime.sessions,
        event_buses: runtime.event_buses,
        setup_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let api = Arc::new(
        crate::command::daemon_api::DaemonApi::from_worker_with_config(state.clone(), config),
    );
    *api.bound_addr.lock().expect("bound address mutex") = Some(addr.to_string());
    *api.listener.lock().expect("listener mutex") = vec![
        crate::command::daemon_api::core::ListenerRecord {
            kind: "local".into(),
            address: addr.to_string(),
            tls: tls.is_some(),
        },
        crate::command::daemon_api::core::ListenerRecord {
            kind: "ipc".into(),
            address: state.paths.daemon().local_endpoint().display().to_string(),
            tls: false,
        },
    ];
    let policy = routes::ListenerPolicy::Network {
        features: crate::command::daemon_api::Feature::ALL
            .into_iter()
            .filter(|f| {
                api.config
                    .lock()
                    .expect("config mutex")
                    .daemon()
                    .network
                    .features
                    .iter()
                    .any(|name| name == f.name())
            })
            .collect(),
    };
    let _local = local::start(
        &state.paths.daemon().local_endpoint(),
        routes::build_router_for_listener(api.clone(), routes::ListenerPolicy::Local),
    )?;
    let app = routes::build_router_for_listener(api, policy);
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
