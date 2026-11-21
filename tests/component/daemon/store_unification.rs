//! One store for every producer: a foreground automation run and the daemon
//! open the same migration-aware database, an independent pair of stores is
//! refused by both without touching either, and API dispatch hosts
//! automation on the store it was given instead of opening its own.
use super::support::Api;
use crate::support;
use awman::command::automation::launch::{AutomationHosting, ForegroundRecord};
use awman::command::commands::agent_setup::AgentLaunchFrontend as _;
use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::daemon_runtime::{DaemonBootstrap, DaemonRuntime};
use awman::data::automation_store::{AutomationStore, NewExecution};
use awman::data::config::{ConfigFile, FileKind};
use awman::data::fs::api_command_log::CommandStateWriter;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::{ApiPaths, DataPaths};
use awman::data::ids::{CommandId, WorkerSessionId};
use awman::data::session::{Session, SessionOpenOptions};
use awman::engine::daemon::core::{TerminalHub, TerminalHubConfig};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::TestSandbox;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const FILES: [&str; 3] = ["awman.db", "awman.db-wal", "awman.db-shm"];

fn session(s: &TestSandbox) -> Session {
    Session::open_at_directory_root(
        s.work().to_path_buf(),
        SessionOpenOptions {
            env: Some(support::env_snapshot(s)),
            ..Default::default()
        },
    )
    .unwrap()
}

/// A populated pre-shared-root database at `<data home>/api/awman.db`.
fn seed(dir: &Path, workdir: &str) -> String {
    let id = WorkerSessionId::new().as_str().to_owned();
    let store = SqliteSessionStore::open(dir).unwrap();
    store
        .insert_session(&id, workdir, "2026-11-20T00:00:00Z")
        .unwrap();
    id
}

fn snapshot(dir: &Path) -> BTreeMap<&'static str, Vec<u8>> {
    FILES
        .into_iter()
        .filter_map(|n| std::fs::read(dir.join(n)).ok().map(|b| (n, b)))
        .collect()
}

#[tokio::test]
async fn legacy_then_foreground_then_daemon_keeps_every_record() {
    let s = support::sandbox(
        "daemon::store_unification::legacy_then_foreground_then_daemon_keeps_every_record",
    );
    let env = support::env_snapshot(&s);
    let legacy = ApiPaths::from_env(&env).unwrap();
    let data = DataPaths::from_env(&env).unwrap();
    let legacy_session = seed(legacy.root(), "/legacy/repo");

    // A foreground automation run on the upgraded installation migrates
    // first, then records itself in the shared store.
    let session = session(&s);
    let (_record, host) = ForegroundRecord::create(&session, "exec prompt").unwrap();
    let foreground_command = host.command_id.clone();
    drop(_record);
    drop(host);
    assert!(legacy.root().join("awman.db.pre-migration").is_file());
    assert!(!legacy.root().join("awman.db").exists());

    // Daemon startup opens the same database: nothing is discarded.
    let api = Api::start(&s).await;
    let store = api.api.runtime.core().session_store().clone();
    assert!(store.get_session(&legacy_session).unwrap().is_some());
    let command = store
        .get_command(foreground_command.as_str())
        .unwrap()
        .expect("foreground command row survives daemon startup");
    assert!(store.get_session(&command.session_id).unwrap().is_some());
    assert!(data.db_path().is_file());
    assert!(!data.migration_journal_path().exists());
    drop(store);
    api.shutdown().await;
}

#[tokio::test]
async fn independent_stores_are_refused_by_foreground_and_daemon_alike() {
    let s = support::sandbox(
        "daemon::store_unification::independent_stores_are_refused_by_foreground_and_daemon_alike",
    );
    let env = support::env_snapshot(&s);
    let legacy = ApiPaths::from_env(&env).unwrap();
    let data = DataPaths::from_env(&env).unwrap();
    seed(legacy.root(), "/legacy/repo");
    seed(data.root(), "/independent/repo");
    let legacy_before = snapshot(legacy.root());
    let current_before = snapshot(data.root());

    let error = ForegroundRecord::create(&session(&s), "exec prompt")
        .err()
        .expect("foreground refuses to run on either database");
    assert!(
        error.to_string().contains("two awman databases exist"),
        "{error}"
    );

    let docker = s.install_fake(support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    let path = awman::data::config::file::global_config_path(&env).unwrap();
    let config = ConfigFile::from_value(
        serde_json::json!({"runtime":{"kind":"docker"},"agents":{"auth":{"refresh":{"enabled":false}}},
            "daemon":{"features":{"engine":{"enabled":true},"squad":{"envPersistence":"none"}}}}),
        FileKind::Global,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(&config.to_value()).unwrap()).unwrap();
    let error = DaemonRuntime::bootstrap(DaemonBootstrap {
        env: env.clone(),
        set: Default::default(),
        squad_run_frontends: awman::frontend::squad::unattended::UnattendedFrontends::shared(),
        push_frontend_factory: awman::frontend::api::ApiFrontendFactory,
        terminal_hub: Arc::new(TerminalHub::new(TerminalHubConfig::default())),
        engine_presentation: Arc::new(awman::command::instance::frontend::PlainPresentation),
    })
    .await
    .err()
    .expect("the daemon refuses to start on either database");
    assert!(
        error.to_string().contains("two awman databases exist"),
        "{error}"
    );

    assert_eq!(snapshot(legacy.root()), legacy_before);
    assert_eq!(snapshot(data.root()), current_before);
    assert!(!data.migration_journal_path().exists());
}

/// A queued API command row in `store` and the writer the worker gives it.
fn queued_command(store: &SqliteSessionStore, api_root: &Path) -> (CommandId, CommandStateWriter) {
    let session = WorkerSessionId::new();
    let command = CommandId::new();
    store
        .insert_session(session.as_str(), "/api/repo", "2026-11-20T00:00:00Z")
        .unwrap();
    store
        .insert_running_lease_command(
            command.as_str(),
            session.as_str(),
            "exec prompt",
            "[]",
            false,
        )
        .unwrap();
    let writer = CommandStateWriter::new(
        ApiPaths::from_root(api_root),
        session.as_str(),
        command.as_str(),
    );
    (command, writer)
}

#[test]
fn api_automation_is_hosted_on_the_injected_shared_store() {
    let s = support::sandbox(
        "daemon::store_unification::api_automation_is_hosted_on_the_injected_shared_store",
    );
    let shared = Arc::new(SqliteSessionStore::open_in_memory().unwrap());
    let api_root = s.tmp().join("api");
    let (command, writer) = queued_command(&shared, &api_root);
    let bus = EventBus::for_attempt(64, 1);
    let frontend = ApiDispatchFrontend::new("exec prompt", &[], bus.sender())
        .with_state_writer(writer)
        .with_shared_store(shared.clone());

    let AutomationHosting::Shared(host) = frontend.automation_host().unwrap() else {
        panic!("a queued API command is never hosted in the foreground");
    };
    assert_eq!(host.command_id, command);

    // A write through the host is visible through the daemon's own handle:
    // the in-memory database is reachable only through that connection.
    let (_db, _store, mut record) = crate::data::automation_store::setup(&s.tmp().join("template"));
    record.command_id = command.clone();
    host.store
        .reserve_execution(NewExecution {
            record: record.clone(),
        })
        .unwrap();
    let seen = AutomationStore::open(shared.clone())
        .unwrap()
        .current_execution(&command)
        .unwrap()
        .expect("execution reserved through the shared host");
    assert_eq!(seen.execution_id, record.execution_id);
    assert_eq!(seen.command_id, command);
    // No database was opened or created by path for the API command.
    assert!(!ApiPaths::from_root(&api_root).db_path().exists());
}

#[test]
fn api_automation_without_a_bound_store_fails_and_never_runs_in_the_foreground() {
    let s = support::sandbox(
        "daemon::store_unification::api_automation_without_a_bound_store_fails_and_never_runs_in_the_foreground",
    );
    let env = support::env_snapshot(&s);
    let shared = SqliteSessionStore::open_in_memory().unwrap();
    let api_root = s.tmp().join("api");
    let bus = EventBus::for_attempt(64, 1);

    // The worker forgot to inject the store: refused, nothing opened.
    let (_, writer) = queued_command(&shared, &api_root);
    let error = ApiDispatchFrontend::new("exec prompt", &[], bus.sender())
        .with_state_writer(writer)
        .automation_host()
        .expect_err("an unbound daemon command cannot host automation");
    assert!(error.to_string().contains("shared store"), "{error}");

    // A command identity that does not parse is refused the same way.
    let writer = CommandStateWriter::new(ApiPaths::from_root(&api_root), "ses_x", "not-a-command");
    let error = ApiDispatchFrontend::new("exec prompt", &[], bus.sender())
        .with_state_writer(writer)
        .with_shared_store(Arc::new(shared))
        .automation_host()
        .expect_err("an invalid command identity cannot host automation");
    assert!(error.to_string().contains("invalid command id"), "{error}");

    // Neither failure fell back to a database by path or to the
    // foreground store.
    assert!(!ApiPaths::from_root(&api_root).db_path().exists());
    assert!(!DataPaths::from_env(&env).unwrap().db_path().exists());

    // Only a frontend with no queued command is an intentional foreground run.
    assert!(matches!(
        ApiDispatchFrontend::new("exec prompt", &[], bus.sender()).automation_host(),
        Ok(AutomationHosting::Foreground)
    ));
}
