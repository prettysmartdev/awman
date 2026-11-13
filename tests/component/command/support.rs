use awman::command::commands::api_server::admission::{
    ApiAdmission, CommandSubmission, IdempotentAdmission, SessionSubmission,
};
use awman::command::commands::api_server::ApiSessionLifecycle;
use awman::command::dispatch::Engines;
use awman::command::session_create::SessionCreateRequest;
use awman::data::api_lifecycle_store::CommandLifecycleStore;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_labels::Labels;
use awman::data::fs::ApiPaths;
use awman::data::session_manager::SessionManager;
use awman_test_support::TestSandbox;
use std::sync::Arc;

/// Real admission/lifecycle over a private database, without starting workers.
/// Uses only ordinary production constructors, never the test-support feature.
pub struct AdmissionFixture {
    pub engines: Engines,
    pub sessions: Arc<SessionManager>,
    pub store: Arc<SqliteSessionStore>,
    pub admission: ApiAdmission,
    pub lifecycle: ApiSessionLifecycle,
    pub paths: ApiPaths,
}

impl AdmissionFixture {
    pub fn new(sandbox: &TestSandbox) -> Self {
        let env = crate::support::env_snapshot(sandbox);
        let engines = Engines::for_env(env.clone()).expect("assemble sandbox engines");
        let paths = ApiPaths::from_root(sandbox.awman_home().join("api"));
        let store = Arc::new(SqliteSessionStore::open_from_paths(&paths).unwrap());
        let sessions = Arc::new(SessionManager::in_memory(env));
        let lifecycle = ApiSessionLifecycle::new(
            store.clone(),
            engines.clone(),
            sessions.clone(),
            paths.clone(),
        );
        let admission = ApiAdmission::new(
            store.clone(),
            CommandLifecycleStore::open(store.clone()).unwrap(),
            paths.clone(),
            engines.clone(),
            sessions.clone(),
            vec![sandbox.work().canonicalize().unwrap()],
            vec![],
        );
        Self {
            engines,
            sessions,
            store,
            admission,
            lifecycle,
            paths,
        }
    }

    pub fn local(&self, sandbox: &TestSandbox) -> SessionSubmission {
        SessionSubmission {
            request: SessionCreateRequest {
                workdir: Some(sandbox.work().to_string_lossy().into_owned()),
                ..Default::default()
            },
            labels: serde_json::from_value(serde_json::json!({"suite/team":"commands"})).unwrap(),
        }
    }

    pub fn journal(&self, key: &str) -> IdempotentAdmission {
        IdempotentAdmission::new(self.store.clone(), "sessions", "component", key)
    }

    pub fn sql(&self, sql: &str) {
        self.store
            .with_conn(|conn| {
                conn.execute_batch(sql)?;
                Ok(())
            })
            .unwrap();
    }
}

pub fn prompt(text: &str) -> CommandSubmission {
    CommandSubmission {
        secrets: None,
        allow_remote_input: false,
        interaction: None,
        subcommand: "exec prompt".into(),
        args: vec![text.into()],
        labels: Labels::empty(),
    }
}
