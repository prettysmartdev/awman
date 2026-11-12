//! The production API router over a caller-owned listener. Configuration names
//! the address already bound by the fixture; no bind-close-rebind reservation.
use crate::support;
use awman::command::{
    daemon_api::DaemonApi,
    daemon_runtime::{DaemonBootstrap, DaemonRuntime},
};
use awman::data::{
    config::{ConfigFile, FileKind},
    daemon_state::StopMode,
};
use awman::engine::daemon::core::{TerminalHub, TerminalHubConfig};
use awman::frontend::api::routes::{build_router_for_listener, ListenerPolicy};
use awman_test_support::TestSandbox;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
pub(super) struct Api {
    pub base: String,
    pub api: Arc<DaemonApi>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

pub(super) fn merge(base: &mut Value, extra: Value) {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            merge(base.entry(key).or_insert(Value::Null), value.clone());
        }
    } else {
        *base = extra;
    }
}
impl Api {
    pub async fn start(sandbox: &TestSandbox) -> Self {
        Self::with_config(sandbox, json!({})).await
    }
    pub async fn with_config(sandbox: &TestSandbox, extra: Value) -> Self {
        let listener = sandbox.ports().tcp_listener();
        let addr = listener.local_addr().unwrap();
        let docker = sandbox.install_fake(support::fake_cli_exe(), "docker");
        awman_test_support::fakes::docker_cli_baseline(&docker);
        let env = support::env_snapshot(sandbox);
        let path = awman::data::config::file::global_config_path(&env).unwrap();
        let mut doc = json!({"runtime":{"kind":"docker"},"agents":{"auth":{"refresh":{"enabled":false}}},
            "daemon":{"features":{"engine":{"enabled":true},"squad":{"envPersistence":"none"}},
            "network":{"listeners":{"component":{"enabled":true,"listen":addr.to_string(),"transport":"loopback_http","features":["engine"],"allowedOrigins":["https://awman.prettysmart.dev"]}}}}});
        merge(&mut doc, extra);
        let config = ConfigFile::from_value(doc, FileKind::Global).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&config.to_value()).unwrap()).unwrap();
        let (runtime, claim) = DaemonRuntime::bootstrap(DaemonBootstrap {
            env,
            set: Default::default(),
            squad_run_frontends: awman::frontend::squad::unattended::UnattendedFrontends::shared(),
            push_frontend_factory: awman::frontend::api::ApiFrontendFactory,
            terminal_hub: Arc::new(TerminalHub::new(TerminalHubConfig::default())),
            engine_presentation: Arc::new(awman::command::instance::frontend::PlainPresentation),
        })
        .await
        .unwrap();
        claim.finish();
        let api = Arc::new(DaemonApi::new(runtime));
        api.runtime.report_listener_bound("component", addr);
        let router = build_router_for_listener(
            api.clone(),
            ListenerPolicy::Network {
                name: "component".into(),
            },
        );
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        let result = Self {
            base: format!("http://{addr}"),
            api,
            stop,
            task,
        };
        assert_eq!(result.get("/v1/health", None).await.0, 200);
        result
    }
    pub async fn get(&self, path: &str, key: Option<&str>) -> (u16, Value) {
        self.request("GET", path, key, None, None).await
    }
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        key: Option<&str>,
        origin: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .request(method.parse().unwrap(), format!("{}{path}", self.base));
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let code = response.status().as_u16();
        (code, response.json().await.unwrap_or(Value::Null))
    }
    pub async fn shutdown(self) {
        self.stop.cancel();
        self.task.await.unwrap();
        self.api.runtime.shutdown(StopMode::Now).await;
    }
}
