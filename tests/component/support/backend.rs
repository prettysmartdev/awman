//! Shared backend fixture. A supervised foreground daemon uses the production
//! bootstrap, router, admission and owner-only transport on sandbox roots.
use awman::command::commands::daemon::{actions::Request, client::DaemonClient};
use awman::data::config::{ConfigFile, FileKind};
use awman::data::fs::DataPaths;
use awman_test_support::{EnvMap, Exit, OwnedChild, Ready, TestSandbox};
use std::{path::PathBuf, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Builtin,
    Daemon,
}
pub struct Backend {
    kind: BackendKind,
    env: EnvMap,
    child: Option<OwnedChild>,
    client: Option<DaemonClient>,
    endpoint: PathBuf,
}
impl Backend {
    pub async fn start(kind: BackendKind, sandbox: &TestSandbox) -> Backend {
        if !sandbox
            .bin()
            .join(if cfg!(windows) {
                "docker.exe"
            } else {
                "docker"
            })
            .exists()
        {
            let docker = sandbox.install_fake(super::fake_cli_exe(), "docker");
            awman_test_support::fakes::docker_cli_baseline(&docker);
        }
        let backend = match kind {
            BackendKind::Builtin => "builtin",
            BackendKind::Daemon => "daemon",
        };
        let env = super::env_snapshot(sandbox);
        let path = awman::data::config::file::global_config_path(&env).unwrap();
        let mut doc = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(error) => panic!("read backend config: {error}"),
        };
        for (key, value) in [
            ("runtime.kind", serde_json::json!("docker")),
            ("agents.auth.refresh.enabled", serde_json::json!(false)),
            ("tui.backend", serde_json::json!(backend)),
            ("daemon.features.engine.enabled", serde_json::json!(true)),
            (
                "daemon.features.squad.envPersistence",
                serde_json::json!("none"),
            ),
        ] {
            awman::data::config::config_json::set_config_field(&mut doc, key, value);
        }
        let config = ConfigFile::from_value(doc, FileKind::Global).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&config.to_value()).unwrap()).unwrap();
        let paths = DataPaths::from_env(&env).unwrap().daemon();
        let endpoint = paths.local_endpoint();
        let mut result = Self {
            kind,
            env: sandbox.env().clone(),
            child: None,
            client: None,
            endpoint,
        };
        if kind == BackendKind::Daemon {
            let child = super::awman(sandbox)
                .args(["daemon", "start"])
                .ready(Ready::FileExists(paths.server_meta_file()))
                .spawn();
            let client = DaemonClient::connect(&env)
                .unwrap()
                .with_timeout(Duration::from_secs(5));
            // server.json is published after bind; a real request proves the
            // request loop and authentication are ready, with no guessed wait.
            client.call(&Request::get("/v1/status")).await.unwrap();
            result.child = Some(child);
            result.client = Some(client);
        }
        let endpoint = result.endpoint.clone();
        sandbox.on_finish(
            "daemon endpoint",
            Box::new(move || {
                #[cfg(unix)]
                if endpoint.exists() {
                    return vec![awman_test_support::leak::Leak::new(
                        "socket",
                        endpoint.display().to_string(),
                    )];
                }
                vec![]
            }),
        );
        result
    }
    pub fn kind(&self) -> BackendKind {
        self.kind
    }
    pub fn env(&self) -> EnvMap {
        self.env.clone()
    }
    pub fn endpoint(&self) -> &std::path::Path {
        &self.endpoint
    }
    pub fn client(&self) -> &DaemonClient {
        self.client.as_ref().expect("daemon backend")
    }
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|child| child.pid())
    }
    pub fn wait_stopped(mut self) {
        let mut child = self.child.take().expect("started daemon backend");
        assert_eq!(child.wait(Duration::from_secs(10)), Exit::Code(0));
        #[cfg(unix)]
        assert!(
            !self.endpoint.exists(),
            "daemon left its owner endpoint behind"
        );
    }
    pub async fn shutdown(mut self) {
        if let Some(mut child) = self.child.take() {
            self.client()
                .call(&Request::get("/v1/status"))
                .await
                .unwrap();
            child.interrupt();
            assert_eq!(child.wait(Duration::from_secs(10)), Exit::Code(0));
            #[cfg(unix)]
            assert!(
                !self.endpoint.exists(),
                "daemon left its owner endpoint behind"
            );
        }
    }
}
