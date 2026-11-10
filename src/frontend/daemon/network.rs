//! Independent network hosts following Layer 2's named desired state.
use crate::command::daemon_api::DaemonApi;
use crate::command::daemon_runtime::{network::ListenerDesired, DaemonRuntime};
use crate::data::config::network::{binds_overlap, ListenerTransport};
use crate::frontend::api::routes::{build_router_for_listener, ListenerPolicy};
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;
mod connections;

pub struct NetworkListener(tokio::task::JoinHandle<()>);
struct Bound {
    desired: ListenerDesired,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    handle: axum_server::Handle<std::net::SocketAddr>,
    streams: CancellationToken,
    clients: Arc<std::sync::atomic::AtomicUsize>,
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
        runtime.listener_host_started();
        Self(tokio::spawn(async move {
            let mut control = runtime.network_control();
            let mut bound: HashMap<String, Bound> = HashMap::new();
            loop {
                let desired = control.borrow_and_update().clone();
                let mut effects = Vec::new();
                let close: Vec<_> = bound
                    .iter()
                    .filter(|(name, b)| {
                        desired.listeners.get(*name).is_none_or(|l| {
                            !l.enabled || l.bind_generation != b.desired.bind_generation
                        })
                    })
                    .map(|(n, _)| n.clone())
                    .collect();
                for name in close {
                    if let Some(old) = bound.remove(&name) {
                        let disconnected = old.clients.load(std::sync::atomic::Ordering::Relaxed);
                        let rebind = desired.listeners.get(&name).is_some_and(|l| l.enabled);
                        old.streams.cancel();
                        old.handle.shutdown();
                        // Wait for release of the listening socket before rebinding its transport.
                        let mut old = old;
                        let _ = (&mut old.task).await;
                        if rebind {
                            runtime.report_listener_rebound(&name, disconnected);
                            effects.push(crate::command::daemon_api::core::ReloadEffect::Rebound {
                                listener: name.clone(),
                                disconnected_clients: disconnected,
                            });
                        }
                    }
                    runtime.report_listener_closed(&name);
                }
                for (name, l) in &desired.listeners {
                    if !l.enabled {
                        continue;
                    }
                    if let Some(current) = bound.get_mut(name) {
                        if current.desired.tls != l.tls {
                            match l.tls_server_config() {
                                Ok(config) => {
                                    if let Some(tls) = &current.tls {
                                        tls.reload_from_config(config);
                                    }
                                    current.desired = l.clone();
                                    effects.push(crate::command::daemon_api::core::ReloadEffect::CertificateReloaded { listener: name.clone() });
                                }
                                Err(error) => runtime.report_listener_error(
                                    name,
                                    l.listen.to_string(),
                                    error.to_string(),
                                ),
                            }
                        }
                        continue;
                    }
                    let result = async {
                        let settings = runtime.listener_settings(name).ok_or("listener removed")?;
                        crate::data::config::network::validate_listeners(
                            &[(name.clone(), settings)].into(),
                        )
                        .map_err(|e| format!("invalid listener: {e:?}"))?;
                        if bound
                            .values()
                            .any(|b| binds_overlap(&b.desired.listen, &l.listen))
                        {
                            return Err("overlapping listener bind".into());
                        }
                        let addresses = tokio::net::lookup_host(l.listen.to_string())
                            .await
                            .map_err(|e| e.to_string())?
                            .collect::<Vec<_>>();
                        if l.transport == ListenerTransport::LoopbackHttp
                            && addresses.iter().any(|a| !a.ip().is_loopback())
                        {
                            return Err("loopback_http resolved to a non-loopback address".into());
                        }
                        let listener = std::net::TcpListener::bind(addresses.as_slice())
                            .map_err(|e| e.to_string())?;
                        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                        let addr = listener.local_addr().map_err(|e| e.to_string())?;
                        let handle = axum_server::Handle::new();
                        let streams = CancellationToken::new();
                        let clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                        let router = build_router_for_listener(
                            api.clone(),
                            ListenerPolicy::Network { name: name.clone() },
                        )
                        .layer(axum::Extension(streams.clone()));
                        let service =
                            router.into_make_service_with_connect_info::<std::net::SocketAddr>();
                        let (tls, task) = match l.transport {
                            ListenerTransport::Tls => {
                                let tls = axum_server::tls_rustls::RustlsConfig::from_config(
                                    l.tls_server_config().map_err(|e| e.to_string())?,
                                );
                                let server = axum_server::from_tcp_rustls(listener, tls.clone())
                                    .map_err(|e| e.to_string())?
                                    .handle(handle.clone())
                                    .map(|inner| connections::CountingAcceptor {
                                        inner,
                                        clients: clients.clone(),
                                        closed: streams.clone(),
                                    });
                                (Some(tls), tokio::spawn(server.serve(service)))
                            }
                            ListenerTransport::LoopbackHttp => {
                                let server = axum_server::from_tcp(listener)
                                    .map_err(|e| e.to_string())?
                                    .handle(handle.clone())
                                    .map(|inner| connections::CountingAcceptor {
                                        inner,
                                        clients: clients.clone(),
                                        closed: streams.clone(),
                                    });
                                (None, tokio::spawn(server.serve(service)))
                            }
                        };
                        runtime.report_listener_bound(name, addr);
                        Ok::<_, String>(Bound {
                            desired: l.clone(),
                            tls,
                            handle,
                            streams,
                            clients,
                            task,
                        })
                    }
                    .await;
                    match result {
                        Ok(listener) => {
                            bound.insert(name.clone(), listener);
                        }
                        Err(error) => runtime.report_listener_error(
                            name,
                            l.listen.to_string(),
                            format!("{}: {error}", l.listen),
                        ),
                    }
                }
                runtime.report_listeners_applied(desired.generation, effects);
                tokio::select! {
                    changed = control.changed() => if changed.is_err() { break; },
                    (name, result) = async {
                        let tasks = bound.iter_mut().map(|(n,b)| async move { (n.clone(), (&mut b.task).await) });
                        let (result, _, _) = futures_util::future::select_all(tasks.map(Box::pin)).await;
                        result
                    }, if !bound.is_empty() => {
                        bound.remove(&name);
                        let error = match result { Ok(Ok(())) => "network listener stopped unexpectedly".into(), Ok(Err(e)) => e.to_string(), Err(e) => e.to_string() };
                        runtime.report_listener_error(&name, desired.listeners[&name].listen.to_string(), error);
                        if control.changed().await.is_err() { break; }
                    }
                }
            }
        }))
    }
}
