//! SQLite session store CRUD and schema compatibility tests.
//!
//! Verifies that SqliteSessionStore creates the expected schema, persists
//! records correctly, and satisfies the on-disk compatibility requirement
//! from WI 0073 §1 (opens databases written by prior amux releases).

use amux::data::fs::headless_db::SqliteSessionStore;

use crate::helpers::IsolatedEnv;

// ─── Store construction ──────────────────────────────────────────────────────

#[test]
fn store_opens_fresh_dir_creates_db_file() {
    let env = IsolatedEnv::new();
    let root = env.headless_root();
    let _store = SqliteSessionStore::open(&root).expect("open store");
    assert!(root.join("amux.db").exists());
}

#[test]
fn store_open_is_idempotent() {
    let env = IsolatedEnv::new();
    let root = env.headless_root();
    let _s1 = SqliteSessionStore::open(&root).expect("first open");
    let _s2 = SqliteSessionStore::open(&root).expect("second open");
}

// ─── Session CRUD ────────────────────────────────────────────────────────────

fn make_store() -> (SqliteSessionStore, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(tmp.path()).unwrap();
    (store, tmp)
}

#[test]
fn insert_and_get_session_roundtrip() {
    let (store, _tmp) = make_store();

    store
        .insert_session("sess-1", "/work/dir", "2026-01-01T00:00:00Z")
        .unwrap();

    let rec = store
        .get_session("sess-1")
        .unwrap()
        .expect("session exists");
    assert_eq!(rec.id, "sess-1");
    assert_eq!(rec.workdir, "/work/dir");
    assert_eq!(rec.created_at, "2026-01-01T00:00:00Z");
    assert_eq!(rec.status, "active");
    assert!(rec.closed_at.is_none());
}

#[test]
fn get_nonexistent_session_returns_none() {
    let (store, _tmp) = make_store();
    let rec = store.get_session("nope").unwrap();
    assert!(rec.is_none());
}

#[test]
fn list_sessions_returns_all_inserted() {
    let (store, _tmp) = make_store();

    store
        .insert_session("a", "/dir-a", "2026-01-01T00:00:00Z")
        .unwrap();
    store
        .insert_session("b", "/dir-b", "2026-01-02T00:00:00Z")
        .unwrap();

    let list = store.list_sessions().unwrap();
    assert_eq!(list.len(), 2);
    let ids: Vec<&str> = list.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"b"));
}

#[test]
fn close_session_updates_status() {
    let (store, _tmp) = make_store();
    store
        .insert_session("sess-1", "/wd", "2026-01-01T00:00:00Z")
        .unwrap();

    let changed = store
        .close_session("sess-1", "2026-01-02T00:00:00Z")
        .unwrap();
    assert!(changed);

    let rec = store.get_session("sess-1").unwrap().expect("exists");
    assert_eq!(rec.status, "closed");
    assert_eq!(rec.closed_at.as_deref(), Some("2026-01-02T00:00:00Z"));
}

#[test]
fn close_already_closed_session_returns_false() {
    let (store, _tmp) = make_store();
    store
        .insert_session("sess-1", "/wd", "2026-01-01T00:00:00Z")
        .unwrap();
    store
        .close_session("sess-1", "2026-01-02T00:00:00Z")
        .unwrap();

    let changed = store
        .close_session("sess-1", "2026-01-03T00:00:00Z")
        .unwrap();
    assert!(!changed);
}

#[test]
fn count_active_sessions_reflects_state() {
    let (store, _tmp) = make_store();

    assert_eq!(store.count_active_sessions().unwrap(), 0);

    store
        .insert_session("a", "/da", "2026-01-01T00:00:00Z")
        .unwrap();
    store
        .insert_session("b", "/db", "2026-01-01T00:00:00Z")
        .unwrap();
    assert_eq!(store.count_active_sessions().unwrap(), 2);

    store.close_session("a", "2026-01-02T00:00:00Z").unwrap();
    assert_eq!(store.count_active_sessions().unwrap(), 1);
}

#[test]
fn list_sessions_by_status_filters_correctly() {
    let (store, _tmp) = make_store();
    store
        .insert_session("a", "/da", "2026-01-01T00:00:00Z")
        .unwrap();
    store
        .insert_session("b", "/db", "2026-01-01T00:00:00Z")
        .unwrap();
    store.close_session("b", "2026-01-02T00:00:00Z").unwrap();

    let active = store.list_sessions_by_status(Some("active")).unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, "a");

    let closed = store.list_sessions_by_status(Some("closed")).unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].id, "b");
}

// ─── Command CRUD ────────────────────────────────────────────────────────────

#[test]
fn insert_and_get_command_roundtrip() {
    let (store, _tmp) = make_store();
    store
        .insert_session("sess-1", "/wd", "2026-01-01T00:00:00Z")
        .unwrap();

    store
        .insert_command(
            "cmd-1",
            "sess-1",
            "exec",
            r#"["prompt","hello"]"#,
            "/logs/cmd-1.log",
        )
        .unwrap();

    let rec = store.get_command("cmd-1").unwrap().expect("command exists");
    assert_eq!(rec.id, "cmd-1");
    assert_eq!(rec.session_id, "sess-1");
    assert_eq!(rec.subcommand, "exec");
    assert_eq!(rec.status, "pending");
    assert!(rec.exit_code.is_none());
}

#[test]
fn update_command_started_and_finished_reflects_change() {
    let (store, _tmp) = make_store();
    store
        .insert_session("sess-1", "/wd", "2026-01-01T00:00:00Z")
        .unwrap();
    store
        .insert_command("cmd-1", "sess-1", "exec", "[]", "/logs/cmd-1.log")
        .unwrap();

    store
        .update_command_started("cmd-1", "2026-01-01T00:01:00Z")
        .unwrap();
    let rec = store.get_command("cmd-1").unwrap().expect("exists");
    assert_eq!(rec.status, "running");

    store
        .update_command_finished("cmd-1", "done", Some(0), "2026-01-01T00:02:00Z")
        .unwrap();
    let rec = store.get_command("cmd-1").unwrap().expect("exists");
    assert_eq!(rec.status, "done");
    assert_eq!(rec.exit_code, Some(0));
    assert!(rec.finished_at.is_some());
}

#[test]
fn has_running_command_reflects_state() {
    let (store, _tmp) = make_store();
    store
        .insert_session("sess-1", "/wd", "2026-01-01T00:00:00Z")
        .unwrap();
    store
        .insert_command("cmd-a", "sess-1", "exec", "[]", "/logs/a.log")
        .unwrap();

    // Pending counts as running (active).
    assert!(store.has_running_command_for_session("sess-1").unwrap());

    // Finish the command.
    store
        .update_command_finished("cmd-a", "done", Some(0), "2026-01-01T00:02:00Z")
        .unwrap();
    assert!(!store.has_running_command_for_session("sess-1").unwrap());
}

// ─── Schema forward-compatibility fixture ────────────────────────────────────

/// This test verifies that a minimal DB written by a prior release (schema v1)
/// can be opened without error. The fixture encodes a sessions + commands table
/// in the exact column layout the old code produced.
#[test]
fn sqlite_upgrade_compat_legacy_fixture_opens_cleanly() {
    use rusqlite::Connection;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("amux.db");

    // Construct a minimal legacy-shaped database directly.
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id         TEXT PRIMARY KEY,
                workdir    TEXT NOT NULL,
                created_at TEXT NOT NULL,
                status     TEXT NOT NULL DEFAULT 'active',
                closed_at  TEXT
            );
            CREATE TABLE commands (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id),
                subcommand  TEXT NOT NULL,
                args        TEXT NOT NULL,
                status      TEXT NOT NULL DEFAULT 'pending',
                exit_code   INTEGER,
                started_at  TEXT,
                finished_at TEXT,
                log_path    TEXT NOT NULL
            );
            INSERT INTO sessions (id, workdir, created_at) VALUES
                ('legacy-sess', '/old/workdir', '2025-01-01T00:00:00Z');
            INSERT INTO commands (id, session_id, subcommand, args, log_path) VALUES
                ('legacy-cmd', 'legacy-sess', 'implement', '[]', '/logs/legacy-cmd.log');",
        )
        .unwrap();
    }

    // Re-open with SqliteSessionStore — should not lose data or error.
    let store = SqliteSessionStore::open(tmp.path()).expect("legacy DB opens");
    let sess = store.get_session("legacy-sess").unwrap().expect("session");
    assert_eq!(sess.workdir, "/old/workdir");

    let cmd = store.get_command("legacy-cmd").unwrap().expect("command");
    assert_eq!(cmd.subcommand, "implement");
}
