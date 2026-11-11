//! Filesystem and database migration behavior in a private component sandbox.

use crate::support;
use awman::data::config::file::{ConfigFile, FileKind};
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::data_paths::{DataPaths, MigrationOutcome, DB_FILENAME};
use awman::data::workflow_state::WorkflowState;
use awman::data::workflow_state_store::WorkflowStateStore;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[test]
fn config_save_reload_and_corruption_recovery_are_confined_to_sandbox() {
    let sandbox = support::sandbox(
        "data::persistence::config_save_reload_and_corruption_recovery_are_confined_to_sandbox",
    );
    let root = sandbox.tmp().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let path = awman::data::config::file::repo_config_path(&root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let config =
        ConfigFile::from_value(json!({"agents":{"default":"codex"}}), FileKind::Repo).unwrap();
    config.save(&path, FileKind::Repo).unwrap();
    assert_eq!(ConfigFile::load(&path, FileKind::Repo).unwrap(), config);

    std::fs::write(&path, b"{ definitely not json").unwrap();
    assert!(ConfigFile::load(&path, FileKind::Repo).is_err());
    config.save(&path, FileKind::Repo).unwrap();
    assert_eq!(ConfigFile::load(&path, FileKind::Repo).unwrap(), config);
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn repo_config_save_refuses_a_symlinked_leaf_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let sandbox = support::sandbox(
        "data::persistence::repo_config_save_refuses_a_symlinked_leaf_without_touching_its_target",
    );
    let root = sandbox.tmp().join("repo");
    let config_dir = root.join(".awman");
    std::fs::create_dir_all(&config_dir).unwrap();
    let target = sandbox.tmp().join("outside-config.json");
    std::fs::write(&target, b"outside bytes").unwrap();
    let path = config_dir.join("config.json");
    symlink(&target, &path).unwrap();
    let config =
        ConfigFile::from_value(json!({"agents":{"default":"codex"}}), FileKind::Repo).unwrap();
    assert!(config.save(&path, FileKind::Repo).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"outside bytes");
    sandbox.finish();
}

#[test]
fn interrupted_database_migration_recovers_from_legacy_source_and_preserves_backup() {
    let sandbox = support::sandbox(
        "data::persistence::interrupted_database_migration_recovers_from_legacy_source_and_preserves_backup",
    );
    let legacy = sandbox.tmp().join("legacy-api");
    std::fs::create_dir_all(&legacy).unwrap();
    let paths = DataPaths::at_root(sandbox.tmp().join("data"));
    std::fs::write(legacy.join(DB_FILENAME), b"authoritative legacy bytes").unwrap();
    std::fs::write(
        legacy.join(format!("{DB_FILENAME}.pre-migration")),
        b"older backup",
    )
    .unwrap();
    paths.ensure_root().unwrap();
    std::fs::write(paths.db_path(), b"partial target").unwrap();

    assert!(matches!(
        paths.migrate_legacy_db(&legacy).unwrap(),
        MigrationOutcome::RecoveredFromInterrupted { .. }
    ));
    assert_eq!(
        std::fs::read(paths.db_path()).unwrap(),
        b"authoritative legacy bytes"
    );
    assert_eq!(
        std::fs::read(legacy.join(format!("{DB_FILENAME}.pre-migration"))).unwrap(),
        b"older backup"
    );
    sandbox.finish();
}

#[test]
fn corrupt_database_is_reported_without_overwriting_the_original_bytes() {
    let sandbox = support::sandbox(
        "data::persistence::corrupt_database_is_reported_without_overwriting_the_original_bytes",
    );
    let path = sandbox.tmp().join("corrupt.db");
    let corrupt = b"not a sqlite database";
    std::fs::write(&path, corrupt).unwrap();
    assert!(SqliteSessionStore::open_at(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    sandbox.finish();
}

#[test]
fn concurrent_workflow_checkpoint_reads_never_observe_partial_json() {
    let sandbox = support::sandbox(
        "data::persistence::concurrent_workflow_checkpoint_reads_never_observe_partial_json",
    );
    let store = WorkflowStateStore::at_git_root(sandbox.tmp().join("repo"));
    let small = WorkflowState::new("wf".into(), &[], "small".into(), None);
    let large = WorkflowState::new("wf".into(), &[], "h".repeat(256 * 1024), None);
    store.save(&small).unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let reader_store = store.clone();
    let reader_done = Arc::clone(&done);
    let reader = std::thread::spawn(move || {
        let mut reads = 0;
        while !reader_done.load(Ordering::Acquire) || reads == 0 {
            let loaded = reader_store.load(None, "wf").unwrap().unwrap();
            assert!(loaded.workflow_hash == "small" || loaded.workflow_hash.len() == 256 * 1024);
            reads += 1;
        }
    });
    for index in 0..32 {
        store
            .save(if index % 2 == 0 { &large } else { &small })
            .unwrap();
    }
    done.store(true, Ordering::Release);
    reader.join().unwrap();
    sandbox.finish();
}

#[test]
fn schema_one_database_keeps_session_and_command_rows_readable() {
    let sandbox = support::sandbox(
        "data::persistence::schema_one_database_keeps_session_and_command_rows_readable",
    );
    let api_root = sandbox.tmp().join("legacy-api");
    std::fs::create_dir_all(&api_root).unwrap();
    let db_path = api_root.join("awman.db");
    {
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, workdir TEXT NOT NULL, created_at TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active', closed_at TEXT
            );
            CREATE TABLE commands (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                subcommand TEXT NOT NULL, args TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending', exit_code INTEGER,
                started_at TEXT, finished_at TEXT, log_path TEXT NOT NULL
            );
            INSERT INTO sessions (id, workdir, created_at)
                VALUES ('legacy-session', '/legacy/work', '2025-01-01T00:00:00Z');
            INSERT INTO commands (id, session_id, subcommand, args, log_path)
                VALUES ('legacy-command', 'legacy-session', 'status', '[]', '/logs/legacy.log');",
            )
            .unwrap();
    }
    let store = SqliteSessionStore::open(&api_root).unwrap();
    let session = store.get_session("ses_legacysession").unwrap().unwrap();
    assert_eq!(session.workdir, "/legacy/work");
    let command = store.get_command("cmd_legacycommand").unwrap().unwrap();
    assert_eq!(command.subcommand, "status");
    // The schema upgrade canonicalizes the old pending spelling to queued.
    assert_eq!(command.status.as_str(), "queued");
    assert_eq!(command.session_id, session.id);
    sandbox.finish();
}

#[test]
fn bounded_keychain_helper_returns_a_ready_result() {
    let sandbox =
        support::sandbox("data::persistence::bounded_keychain_helper_returns_a_ready_result");
    let result =
        awman::data::fs::daemon_env::call_with_cap(std::time::Duration::from_secs(2), || 42);
    assert_eq!(result, Some(42));
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn builtin_image_leases_isolate_mutation_per_image() {
    let sandbox =
        support::sandbox("data::persistence::builtin_image_leases_isolate_mutation_per_image");
    let state = awman::data::builtin_state::BuiltinState::resolve(
        &sandbox.tmp().join("builtin-state"),
        |_| Ok(()),
    )
    .unwrap();
    let shared = state.image_lease("agent:1", false).unwrap();
    assert!(state.image_lease("agent:1", true).is_err());
    assert!(state.image_lease("other:1", true).is_ok());
    drop(shared);
    assert!(state.image_lease("agent:1", true).is_ok());
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn concurrent_builtin_state_resolution_returns_one_private_root() {
    let sandbox = support::sandbox(
        "data::persistence::concurrent_builtin_state_resolution_returns_one_private_root",
    );
    let root = sandbox.tmp().join("shared-state");
    std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            awman::data::builtin_state::BuiltinState::resolve(&root, |_| Ok(())).unwrap()
        });
        let right = scope.spawn(|| {
            awman::data::builtin_state::BuiltinState::resolve(&root, |_| Ok(())).unwrap()
        });
        assert_eq!(left.join().unwrap().home, right.join().unwrap().home);
    });
    sandbox.finish();
}

#[test]
fn migration_preserves_real_sqlite_session_and_command_rows() {
    let sandbox = support::sandbox(
        "data::persistence::migration_preserves_real_sqlite_session_and_command_rows",
    );
    let legacy = sandbox.tmp().join("legacy");
    let paths = DataPaths::at_root(sandbox.tmp().join("current"));
    let session_id = "ses_11111111111141118111111111111111";
    let command_id = "cmd_22222222222242228222222222222222";
    let store = SqliteSessionStore::open(&legacy).unwrap();
    store
        .insert_session(session_id, "/legacy/repo", "2026-08-27T00:00:00Z")
        .unwrap();
    store
        .insert_command(
            command_id,
            session_id,
            "status",
            "[\"--json\"]",
            "/legacy/output.log",
        )
        .unwrap();
    let session = store.get_session(session_id).unwrap().unwrap();
    let command = store.get_command(command_id).unwrap().unwrap();
    drop(store);
    assert!(matches!(
        paths.migrate_legacy_db(&legacy).unwrap(),
        MigrationOutcome::Migrated { .. }
    ));
    let migrated = SqliteSessionStore::open_at(&paths.db_path()).unwrap();
    assert_eq!(migrated.get_session(session_id).unwrap().unwrap(), session);
    assert_eq!(migrated.get_command(command_id).unwrap().unwrap(), command);
    drop(migrated);
    sandbox.finish();
}
