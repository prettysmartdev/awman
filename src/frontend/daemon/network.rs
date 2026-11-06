//! TLS-only network host following Layer 2's live desired state.
use crate::command::daemon_api::DaemonApi;
use crate::command::daemon_runtime::{DaemonRuntime, NetworkDesired};
use crate::frontend::api::routes::{build_router_for_listener, ListenerPolicy};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct NetworkListener(tokio::task::JoinHandle<()>);
struct Bound {
    desired: NetworkDesired,
    tls: axum_server::tls_rustls::RustlsConfig,
    handle: axum_server::Handle<std::net::SocketAddr>,
    streams: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Drop for Bound {
    fn drop(&mut self) {
        self.streams.cancel();
        self.handle.shutdown();
        self.task.abort();
    }
}
impl Drop for NetworkListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl NetworkListener {
    pub fn start(runtime: Arc<DaemonRuntime>, api: Arc<DaemonApi>) -> Self {
        Self(tokio::spawn(async move {
            let mut control = runtime.network_control();
            let mut bound: Option<Bound> = None;
            loop {
                let desired = control.borrow_and_update().clone();
                if !desired.enabled {
                    if let Some(mut old) = bound.take() {
                        old.streams.cancel();
                        old.handle.graceful_shutdown(Some(Duration::from_secs(5)));
                        let _ = (&mut old.task).await;
                    }
                    runtime.report_network_closed();
                } else if let Some(current) = bound.as_mut() {
                    if current.desired.tls != desired.tls {
                        match desired.tls_server_config() {
                            Ok(config) => {
                                current.tls.reload_from_config(config);
                                current.desired = desired.clone();
                                tracing::info!("daemon network TLS certificate reloaded");
                            }
                            Err(error) => runtime.report_network_error(
                                desired.listen.to_string(),
                                error.to_string(),
                            ),
                        }
                    }
                } else {
                    let result = async {
                        let config = desired.tls_server_config().map_err(|e| e.to_string())?;
                        let addresses = tokio::net::lookup_host(desired.listen.to_string())
                            .await
                            .map_err(|e| e.to_string())?
                            .collect::<Vec<_>>();
                        let listener = std::net::TcpListener::bind(addresses.as_slice())
                            .map_err(|e| e.to_string())?;
                        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                        let addr = listener.local_addr().map_err(|e| e.to_string())?;
                        let tls = axum_server::tls_rustls::RustlsConfig::from_config(config);
                        let handle = axum_server::Handle::new();
                        let streams = CancellationToken::new();
                        let router =
                            build_router_for_listener(api.clone(), ListenerPolicy::Network)
                                .layer(axum::Extension(streams.clone()));
                        let server = axum_server::from_tcp_rustls(listener, tls.clone())
                            .map_err(|e| e.to_string())?
                            .handle(handle.clone());
                        let task = tokio::spawn(server.serve(
                            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                        ));
                        runtime.report_network_bound(addr);
                        Ok::<_, String>(Bound {
                            desired: desired.clone(),
                            tls,
                            handle,
                            streams,
                            task,
                        })
                    }
                    .await;
                    match result {
                        Ok(listener) => bound = Some(listener),
                        Err(error) => runtime.report_network_error(
                            desired.listen.to_string(),
                            format!("{}: {error}", desired.listen),
                        ),
                    }
                }
                tokio::select! {
                    changed = control.changed() => if changed.is_err() { break; },
                    result = async { (&mut bound.as_mut().expect("guarded bound listener").task).await }, if bound.is_some() => {
                        let error = match result {
                            Ok(Ok(())) => "network listener stopped unexpectedly".into(),
                            Ok(Err(e)) => e.to_string(),
                            Err(e) => e.to_string(),
                        };
                        bound.take();
                        runtime.report_network_error(desired.listen.to_string(), error);
                        // Retry on the next configuration change, without a busy loop.
                        if control.changed().await.is_err() { break; }
                    }
                }
            }
        }))
    }
}
