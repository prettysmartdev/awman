//! The daemon process host: bootstraps the unified runtime with this layer's
//! presentation adapters and serves its listeners until shutdown.
//!
//! Both listeners serve the same route tree; the network host follows live policy.
mod network;
use crate::command::daemon_api::DaemonApi;
use crate::command::daemon_runtime::{DaemonBootstrap, DaemonRuntime, LocalTransport};
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::daemon_state::StopMode;
use crate::data::fs::daemon_sidecar::{ServerSidecar, SidecarTransportKind};
use crate::engine::daemon::core::{TerminalHub, TerminalHubConfig};
use crate::engine::daemon::DaemonClaim;
use crate::frontend::api::{local, routes};
use std::sync::Arc;

/// Bootstrap the daemon for `env` and serve it until a shutdown signal, a
/// `daemon stop`, or the last ephemeral feature idling out.
pub async fn run(
    env: EnvSnapshot,
    set: crate::data::config::overrides::ConfigOverrides,
) -> Result<(), CommandError> {
    let (runtime, claim) = DaemonRuntime::bootstrap(DaemonBootstrap {
        env,
        set,
        squad_run_frontends: crate::frontend::squad::unattended::UnattendedFrontends::shared(),
        push_frontend_factory: crate::frontend::api::ApiFrontendFactory,
        terminal_hub: Arc::new(TerminalHub::new(TerminalHubConfig::default())),
        engine_presentation: Arc::new(crate::frontend::tui::command_frontend::TuiPresentation),
    })
    .await?;
    serve(runtime, claim).await
}

/// Serve a bootstrapped runtime. The local listener is bound while `claim`
/// still holds the startup lock, then `server.json` is written and the claim
/// released.
pub async fn serve(runtime: Arc<DaemonRuntime>, claim: DaemonClaim) -> Result<(), CommandError> {
    // Install handlers before publishing an endpoint. Another runtime thread
    // can serve status immediately after bind, before this task next polls.
    let mut signals = match ShutdownSignals::new() {
        Ok(signals) => signals,
        Err(error) => {
            runtime.shutdown(StopMode::Now).await;
            return Err(error);
        }
    };
    let api = Arc::new(DaemonApi::new(runtime.clone()));
    let paths = runtime.daemon_paths().clone();
    let bound = local::start_claimed(
        &claim,
        &paths.local_endpoint(),
        routes::build_router_for_listener(api.clone(), routes::ListenerPolicy::Local),
    );
    let (listener, bind) = match bound {
        Ok(bound) => bound,
        Err(error) => {
            runtime.shutdown(StopMode::Now).await;
            return Err(error);
        }
    };
    runtime.set_local_listener(
        match bind.transport {
            SidecarTransportKind::UnixSocket => LocalTransport::UnixSocket,
            SidecarTransportKind::NamedPipe => LocalTransport::NamedPipe,
        },
        bind.address.clone(),
    );
    let network = network::NetworkListener::start(runtime.clone(), api);
    if let Err(error) = ServerSidecar::current(claim.pid(), bind.sidecar_transport())
        .write(&paths.server_meta_file())
    {
        drop(listener);
        runtime.shutdown(StopMode::Now).await;
        return Err(error.into());
    }
    claim.finish();
    tracing::info!(address = %bind.address, "awman daemon listening");

    let exit = runtime.exit_signal();
    tokio::select! {
            _ = exit.cancelled() => {
                tracing::info!("awman daemon exit requested");
            }
            _ = signals.wait() => {
                tracing::info!("awman daemon stopping on signal");
            }
    }
    tokio::select! {
        _ = runtime.shutdown(StopMode::Drain) => {},
        _ = signals.wait() => runtime.shutdown(StopMode::Now).await,
    }
    drop(network);
    listener.shutdown().await;
    drop(listener);
    tracing::info!("awman daemon stopped");
    Ok(())
}

/// SIGINT or SIGTERM (Ctrl-C / Ctrl-Break on Windows).
struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    terminate: tokio::signal::windows::CtrlBreak,
}
impl ShutdownSignals {
    fn new() -> Result<Self, CommandError> {
        let error =
            |e| CommandError::Other(format!("cannot install daemon shutdown handlers: {e}"));
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt()).map_err(error)?,
                terminate: signal(SignalKind::terminate()).map_err(error)?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c().map_err(error)?,
                terminate: tokio::signal::windows::ctrl_break().map_err(error)?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = error;
            Ok(Self {})
        }
    }
    async fn wait(&mut self) {
        #[cfg(any(unix, windows))]
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
