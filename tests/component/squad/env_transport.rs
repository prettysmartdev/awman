use crate::support::{
    self,
    backend::{Backend, BackendKind},
};
use awman::command::commands::{
    daemon::actions::Request,
    squad::{client::DaemonSquadClient, gateway::TaskGateway},
};
use awman::data::fs::{DataPaths, MountScope, Task, TaskStatus, TaskStore};
use awman::engine::squad::env_state::{coverage_digest, Salt};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_shell_sync_rotates_only_declared_values_and_read_only_queries_do_not_push() {
    let sandbox = support::sandbox("squad::env_transport::child_shell_sync_rotates_only_declared_values_and_read_only_queries_do_not_push");
    let env = support::env_snapshot(&sandbox);
    let paths = DataPaths::from_env(&env).unwrap();
    std::fs::create_dir_all(paths.db_path().parent().unwrap()).unwrap();
    let store = TaskStore::open(&paths.db_path()).unwrap();
    store.migrate().unwrap();
    let now = chrono::Utc::now();
    store
        .create(&Task {
            id: awman::data::ids::TaskId::new(),
            name: "env-sync".into(),
            description: "paused: never launches an agent".into(),
            repo_scope: sandbox.work(),
            mount_scope: MountScope::Directory,
            overlays: vec!["env(SLICE_TOKEN)".into(), "env(SLICE_ABSENT)".into()],
            interval_secs: 3600,
            status: TaskStatus::Paused,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: vec![],
            prerequisites: None,
        })
        .unwrap();
    let config = awman::data::config::file::global_config_path(&env).unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        config,
        json!({"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}})
            .to_string(),
    )
    .unwrap();
    let backend = Backend::start(BackendKind::Daemon, &sandbox).await;
    let client = DaemonSquadClient::new(backend.client().clone());
    let before = client.env_coverage().await.unwrap();
    assert!(before.required.iter().all(|entry| entry.digest.is_none()));
    let salt = Salt::from_hex(&before.salt).unwrap();
    for value in ["first-private-value", "rotated-private-value"] {
        sandbox.redact(value);
        let output = support::awman(&sandbox)
            .args(["squad", "env"])
            .env("SLICE_TOKEN", value)
            .env("UNDECLARED_TOKEN", "never-sent")
            .run();
        output.assert_success();
        assert!(!output.stdout_text().contains(value));
        assert!(!output.stderr_text().contains(value));
        let coverage = client.env_coverage().await.unwrap();
        let held = coverage
            .required
            .iter()
            .find(|entry| entry.name == "SLICE_TOKEN")
            .unwrap();
        assert_eq!(
            held.digest,
            Some(coverage_digest(&salt, "SLICE_TOKEN", value))
        );
        assert!(coverage
            .required
            .iter()
            .find(|entry| entry.name == "SLICE_ABSENT")
            .unwrap()
            .digest
            .is_none());
        assert!(coverage
            .required
            .iter()
            .all(|entry| entry.name != "UNDECLARED_TOKEN"));
        let provided = held.last_provided_at;
        // Matching digests are steady state: no push or timestamp change.
        support::awman(&sandbox)
            .args(["squad", "env"])
            .env("SLICE_TOKEN", value)
            .run()
            .assert_success();
        assert_eq!(
            client
                .env_coverage()
                .await
                .unwrap()
                .required
                .iter()
                .find(|entry| entry.name == "SLICE_TOKEN")
                .unwrap()
                .last_provided_at,
            provided
        );
    }
    let held = client.env_coverage().await.unwrap();
    let resolver =
        awman::command::commands::squad::supervisor::DaemonResolver::from_env(&env).unwrap();
    assert!(resolver
        .gateway_for(awman::command::dispatch::catalogue::GatewayNeed::IfRunning)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        serde_json::to_value(client.env_coverage().await.unwrap()).unwrap(),
        serde_json::to_value(held).unwrap()
    );
    let status = backend
        .client()
        .call(&Request::get("/v1/status"))
        .await
        .unwrap();
    assert_eq!(status["pid"].as_u64(), backend.pid().map(u64::from));
    backend.shutdown().await;
    drop(store);
    sandbox.finish();
}
