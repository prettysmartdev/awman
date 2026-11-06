//! Squad listener over the shared resource route tree.
pub mod unattended;
use crate::command::commands::squad::daemon_runtime::SquadDaemonHandles;
use crate::command::daemon_api::DaemonApi;
use crate::command::error::CommandError;
use crate::frontend::api::{
    routes,
    serve::{self, ServeOptions},
};
use std::sync::Arc;

pub async fn serve(handles: SquadDaemonHandles) -> Result<(), CommandError> {
    let handles = Arc::new(handles);
    let requested = handles.bind_addr();
    let api = Arc::new(DaemonApi::from_squad(handles.clone()));
    let router = routes::build_router_for_listener(
        api.clone(),
        routes::ListenerPolicy::Network {
            features: vec![crate::command::daemon_api::Feature::Squad],
        },
    );
    let paths =
        crate::data::fs::SquadPaths::from_env(api.config.lock().expect("config mutex").env())?;
    let local_endpoint = paths.daemon().local_endpoint().display().to_string();
    let _local = crate::frontend::api::local::start(
        &paths.daemon().local_endpoint(),
        routes::build_router_for_listener(api.clone(), routes::ListenerPolicy::Local),
    )?;
    let bound_api = api.clone();
    let bound_handles = handles.clone();
    let result = serve::serve_router_with_bound(
        router,
        ServeOptions {
            addr: requested,
            tls: None,
            shutdown_grace: std::time::Duration::from_secs(30),
        },
        move |addr| {
            let endpoint = bound_handles.publish_endpoint(addr)?;
            *bound_api.bound_addr.lock().expect("bound address mutex") = Some(endpoint.clone());
            *bound_api.listener.lock().expect("listener mutex") = vec![
                crate::command::daemon_api::core::ListenerRecord {
                    kind: "local".into(),
                    address: addr.to_string(),
                    tls: false,
                },
                crate::command::daemon_api::core::ListenerRecord {
                    kind: "ipc".into(),
                    address: local_endpoint,
                    tls: false,
                },
            ];
            tracing::info!(endpoint, "squad daemon listening");
            Ok(())
        },
    )
    .await;
    handles.shutdown().await;
    result
}
