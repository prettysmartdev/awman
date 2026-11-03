//! Headless parity tests (WI 0073).
//!
//! routes.rs       — verifies the route table and API paths.
//! auth_modes.rs   — auth-mode-related path/type smoke checks.
//! live_server.rs  — boots the real Axum router on an ephemeral port and
//!                   hits it with reqwest. Tests are prefixed `real_network_`
//!                   so `make test-fast` can skip them.
//!
//! SSE wire-format and WebSocket tests against a fully booted `awman api
//! start` subprocess are deferred to WI 0076.

#[path = "../helpers/mod.rs"]
mod helpers;

#[cfg(unix)]
#[path = "../helpers/api_process.rs"]
mod api_process;
#[cfg(unix)]
#[path = "../helpers/git_daemon.rs"]
mod git_daemon;

/// Serve the real router on an ephemeral loopback port. The listener is
/// bound before this returns, so requests queue until `axum::serve` accepts
/// them; no settle delay is needed. `None` when loopback cannot be bound.
pub(crate) async fn spawn_router(
    state: std::sync::Arc<awman::frontend::api::routes::AppState>,
) -> Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let app = awman::frontend::api::routes::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some((addr, handle))
}

mod auth_modes;
mod http_core;
mod live_server;
mod rename_0077;
mod routes;
mod wi_0078;
mod wi_0079;
mod wi_0097;
mod wi_0097_architecture;
mod wi_0132;
mod wi_0133_remote;
mod wi_0134;
mod wi_0135;
mod wi_0136;
mod wi_0137_results;
mod wi_0137_surface;
mod wi_0138;
