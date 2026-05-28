//! WI-0079 test suite: Queue-and-Worker Execution System.
//!
//! Test categories:
//!   - Queue unit tests (SqliteSessionStore queue operations)
//!   - Concurrency unit tests (atomic claim, session-exclusive execution)
//!   - HTTP integration tests (route behavior under queue semantics)
//!   - Session lifecycle tests (DELETE drain-and-kill)
//!   - WorkflowViewState conversion unit tests
//!   - Configuration unit tests

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use awman::command::dispatch::Engines;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::routes::{build_router, AppState, AuthMode};

// ─── Test helpers ─────────────────────────────────────────────────────────────

fn make_store() -> (tempfile::TempDir, SqliteSessionStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(tmp.path()).unwrap();
    (tmp, store)
}

fn make_app_state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().expect("ensure_root");
    let store = SqliteSessionStore::open(paths.root()).expect("open sqlite");

    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let git_engine = Arc::new(GitEngine::new());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, paths.clone()));
    let workflow_state_store = Arc::new(EngineWorkflowStateStore::at_git_root(paths.root()));

    let engines = Engines {
        runtime,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
    };

    Arc::new(AppState {
        store: Arc::new(store),
        paths,
        workdirs: vec![],
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

async fn spawn_router(
    state: Arc<AppState>,
) -> Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Some((addr, handle))
}

// Insert a session directly into the store with status='active' and setup_status='ready'.
fn insert_ready_session(store: &SqliteSessionStore, session_id: &str, workdir: &str) {
    store
        .insert_session_full(
            session_id,
            workdir,
            &chrono::Utc::now().to_rfc3339(),
            "ready",
            "local",
            None,
        )
        .unwrap();
}

// ─── Queue unit tests ─────────────────────────────────────────────────────────

/// Enqueue 3 commands, claim them all — should come back in FIFO order.
/// 4th claim returns None.
#[test]
fn enqueue_and_claim_fifo_order() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    // Stagger inserts with tiny delays so queued_at is strictly ordered.
    store
        .enqueue_command("c1", "s1", "exec workflow", r#"["a.toml"]"#, "/logs/c1")
        .unwrap();
    // sleep 2ms to ensure distinct timestamps
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .enqueue_command("c2", "s1", "exec workflow", r#"["b.toml"]"#, "/logs/c2")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .enqueue_command("c3", "s1", "exec workflow", r#"["c.toml"]"#, "/logs/c3")
        .unwrap();

    let w = "worker-1";
    let claimed1 = store
        .claim_next_command(w)
        .unwrap()
        .expect("c1 must be claimed");
    assert_eq!(claimed1.id, "c1", "first claim must be c1 (FIFO)");
    assert_eq!(claimed1.status, "running");
    assert_eq!(claimed1.worker_id.as_deref(), Some(w));

    // With c1 running, session s1 is busy — no more claims for s1 yet.
    let claimed_none = store.claim_next_command(w).unwrap();
    assert!(
        claimed_none.is_none(),
        "session has a running command — no other claim should succeed"
    );

    // Complete c1, then the next queued command becomes claimable.
    store.complete_command("c1", "done", Some(0), None).unwrap();

    let claimed2 = store
        .claim_next_command(w)
        .unwrap()
        .expect("c2 must be claimed");
    assert_eq!(claimed2.id, "c2", "second claim must be c2 (FIFO)");
    store.complete_command("c2", "done", Some(0), None).unwrap();

    let claimed3 = store
        .claim_next_command(w)
        .unwrap()
        .expect("c3 must be claimed");
    assert_eq!(claimed3.id, "c3");
    store.complete_command("c3", "done", Some(0), None).unwrap();

    // Queue is empty.
    let nothing = store.claim_next_command(w).unwrap();
    assert!(
        nothing.is_none(),
        "queue is empty — fourth claim must be None"
    );
}

/// Spawn N threads all racing to claim from N commands simultaneously.
/// Each command must be claimed by exactly one worker (no duplicates).
#[test]
fn atomic_claim_no_duplicates() {
    let (tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    let store = Arc::new(store);

    // Create 4 sessions so each command can be claimed independently
    // (one command per session avoids the session-exclusion constraint).
    for i in 0..4u32 {
        store
            .insert_session_full(
                &format!("s{i}"),
                &format!("/wd/{i}"),
                &ts,
                "ready",
                "local",
                None,
            )
            .unwrap();
        store
            .enqueue_command(
                &format!("c{i}"),
                &format!("s{i}"),
                "exec prompt",
                "[]",
                &format!("{}/c{i}.log", tmp.path().display()),
            )
            .unwrap();
    }

    // Spawn 4 threads each calling claim_next_command once.
    let handles: Vec<_> = (0..4u32)
        .map(|i| {
            let store_ref = Arc::clone(&store);
            std::thread::spawn(move || {
                store_ref
                    .claim_next_command(&format!("worker-{i}"))
                    .unwrap()
            })
        })
        .collect();

    let mut claimed_ids: Vec<String> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap())
        .map(|c| c.id)
        .collect();

    claimed_ids.sort();

    // All 4 commands claimed, each exactly once.
    assert_eq!(
        claimed_ids,
        vec!["c0", "c1", "c2", "c3"],
        "each command must be claimed by exactly one worker; got {claimed_ids:?}"
    );
}

/// Two workers compete for the same session's queue.
/// Only one command may be running per session at a time.
#[test]
fn session_exclusive_execution_one_running_at_a_time() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    let store = Arc::new(store);

    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/logs/c1")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .enqueue_command("c2", "s1", "exec workflow", "[]", "/logs/c2")
        .unwrap();

    // Two workers race for the same session.
    let s1 = Arc::clone(&store);
    let s2 = Arc::clone(&store);
    let h1 = std::thread::spawn(move || s1.claim_next_command("worker-A").unwrap());
    let h2 = std::thread::spawn(move || s2.claim_next_command("worker-B").unwrap());

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    // Exactly one claim succeeds; the other returns None.
    let (claimed, _empty) = match (r1, r2) {
        (Some(c), None) => (c, ()),
        (None, Some(c)) => (c, ()),
        (Some(_), Some(_)) => panic!("both workers claimed — session exclusion violated"),
        (None, None) => panic!("neither worker claimed — unexpected"),
    };
    assert_eq!(
        claimed.id, "c1",
        "only the first-queued command is claimable"
    );
    assert_eq!(claimed.status, "running");
}

/// One command per distinct session — both should be claimable concurrently.
#[test]
fn cross_session_concurrency_both_claimed() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    let store = Arc::new(store);

    for (sid, cid) in [("sA", "cA"), ("sB", "cB")] {
        store
            .insert_session_full(sid, "/wd", &ts, "ready", "local", None)
            .unwrap();
        store
            .enqueue_command(cid, sid, "exec prompt", "[]", "/logs/c")
            .unwrap();
    }

    let s1 = Arc::clone(&store);
    let s2 = Arc::clone(&store);
    let h1 = std::thread::spawn(move || s1.claim_next_command("worker-1").unwrap());
    let h2 = std::thread::spawn(move || s2.claim_next_command("worker-2").unwrap());

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    // Both should be claimed (different sessions don't block each other).
    assert!(r1.is_some(), "worker-1 must claim a command");
    assert!(r2.is_some(), "worker-2 must claim a command");

    let ids: Vec<String> = [r1.unwrap().id, r2.unwrap().id]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(ids, vec!["cA", "cB"], "both commands must be claimed once");
}

/// Stale command recovery resets 'running' commands older than the timeout.
#[test]
fn stale_command_recovery_resets_old_running_commands() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    // Insert a command directly with running status and a very old started_at.
    let conn_ref = store.with_conn(|conn| {
        conn.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                     started_at, worker_id, queued_at)
             VALUES ('stale-cmd', 's1', 'exec workflow', '[]', 'running', '/logs/stale',
                     '2020-01-01T00:00:00Z', 'old-worker', '2020-01-01T00:00:00Z')",
            [],
        )
        .map_err(awman::data::error::DataError::from)
    });
    conn_ref.unwrap();

    // Verify it's running before recovery.
    let cmd = store.get_command("stale-cmd").unwrap().unwrap();
    assert_eq!(cmd.status, "running");

    // Recover with a 1-second timeout — this command is years old.
    let recovered = store.recover_stale_commands(1).unwrap();
    assert_eq!(
        recovered,
        vec!["stale-cmd"],
        "stale command must be recovered"
    );

    // Now it should be 'queued' with worker_id and started_at cleared.
    let cmd_after = store.get_command("stale-cmd").unwrap().unwrap();
    assert_eq!(
        cmd_after.status, "queued",
        "recovered command must have status='queued'"
    );
    assert!(
        cmd_after.worker_id.is_none(),
        "worker_id must be cleared after recovery"
    );
    assert!(
        cmd_after.started_at.is_none(),
        "started_at must be cleared after recovery"
    );
}

/// A fresh command with recent started_at must NOT be touched by recovery.
#[test]
fn stale_command_recovery_does_not_touch_fresh_running_commands() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/logs/c1")
        .unwrap();
    // Claim (sets started_at to now).
    store.claim_next_command("worker-1").unwrap();

    // Recover with a very large timeout — c1 was just claimed and should not be recovered.
    let recovered = store.recover_stale_commands(3600).unwrap();
    assert!(
        recovered.is_empty(),
        "fresh command must not be recovered; got {recovered:?}"
    );

    let cmd = store.get_command("c1").unwrap().unwrap();
    assert_eq!(cmd.status, "running");
}

/// Legacy commands with status='pending' are treated as queued by
/// claim_next_command (backward compatibility).
#[test]
fn backward_compat_legacy_pending_commands_are_claimable() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    // Insert legacy command via old insert_command (status='pending').
    store
        .insert_command("legacy-c1", "s1", "exec workflow", "[]", "/logs/legacy")
        .unwrap();

    let cmd = store.get_command("legacy-c1").unwrap().unwrap();
    assert_eq!(cmd.status, "pending");

    // Workers must be able to claim pending commands.
    let claimed = store
        .claim_next_command("worker-1")
        .unwrap()
        .expect("legacy pending command must be claimable");

    assert_eq!(claimed.id, "legacy-c1");
    assert_eq!(claimed.status, "running");
}

/// count_queued_for_session returns accurate counts.
#[test]
fn count_queued_for_session_is_accurate() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    assert_eq!(store.count_queued_for_session("s1").unwrap(), 0);

    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/logs/c1")
        .unwrap();
    store
        .enqueue_command("c2", "s1", "exec workflow", "[]", "/logs/c2")
        .unwrap();
    store
        .enqueue_command("c3", "s1", "exec workflow", "[]", "/logs/c3")
        .unwrap();
    assert_eq!(store.count_queued_for_session("s1").unwrap(), 3);

    // Claim one — depth drops to 2 (it's now 'running').
    store.claim_next_command("w1").unwrap();
    assert_eq!(store.count_queued_for_session("s1").unwrap(), 2);
}

/// cancel_queued_for_session marks all queued commands as cancelled and
/// returns their IDs.
#[test]
fn cancel_queued_for_session_cancels_all_queued() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/l1")
        .unwrap();
    store
        .enqueue_command("c2", "s1", "exec workflow", "[]", "/l2")
        .unwrap();
    store
        .enqueue_command("c3", "s1", "exec workflow", "[]", "/l3")
        .unwrap();

    let mut cancelled = store.cancel_queued_for_session("s1").unwrap();
    cancelled.sort();
    assert_eq!(cancelled, vec!["c1", "c2", "c3"]);

    for id in &["c1", "c2", "c3"] {
        let cmd = store.get_command(id).unwrap().unwrap();
        assert_eq!(cmd.status, "cancelled", "{id} must be 'cancelled'");
        assert!(
            cmd.finished_at.is_some(),
            "{id} must have finished_at set after cancellation"
        );
    }
}

/// queue_position_for_command returns 0-indexed position within the session queue.
#[test]
fn queue_position_for_command_is_zero_indexed() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/l1")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .enqueue_command("c2", "s1", "exec workflow", "[]", "/l2")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .enqueue_command("c3", "s1", "exec workflow", "[]", "/l3")
        .unwrap();

    let pos0 = store.queue_position_for_command("c1", "s1").unwrap();
    let pos1 = store.queue_position_for_command("c2", "s1").unwrap();
    let pos2 = store.queue_position_for_command("c3", "s1").unwrap();

    assert_eq!(pos0, Some(0), "c1 must be at position 0");
    assert_eq!(pos1, Some(1), "c2 must be at position 1");
    assert_eq!(pos2, Some(2), "c3 must be at position 2");
}

/// Queue position for a running/done command returns None.
#[test]
fn queue_position_is_none_for_non_queued_commands() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/l1")
        .unwrap();
    store.claim_next_command("w1").unwrap(); // now 'running'

    let pos = store.queue_position_for_command("c1", "s1").unwrap();
    assert!(
        pos.is_none(),
        "running command must have no queue_position; got {pos:?}"
    );
}

/// 100 commands across 10 sessions — drain all without deadlock or double-execution.
#[test]
fn queue_depth_under_load_no_duplicates() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    let store = Arc::new(store);

    for sess_idx in 0..10u32 {
        let sid = format!("sess-{sess_idx}");
        store
            .insert_session_full(
                &sid,
                &format!("/wd/{sess_idx}"),
                &ts,
                "ready",
                "local",
                None,
            )
            .unwrap();
        for cmd_idx in 0..10u32 {
            let cid = format!("cmd-{sess_idx}-{cmd_idx}");
            store
                .enqueue_command(&cid, &sid, "exec prompt", "[]", &format!("/logs/{cid}"))
                .unwrap();
        }
    }

    // Drain with 4 concurrent workers.
    let mut all_claimed: Vec<String> = Vec::new();
    // Each session has 10 commands that must run serially, so we drain
    // in a sequential loop but multiple sessions progress simultaneously.
    loop {
        let mut batch_claimed = Vec::new();
        for wid in 0..4usize {
            if let Some(cmd) = store.claim_next_command(&format!("loader-{wid}")).unwrap() {
                batch_claimed.push(cmd.id.clone());
                store
                    .complete_command(&cmd.id, "done", Some(0), None)
                    .unwrap();
            }
        }
        if batch_claimed.is_empty() {
            break;
        }
        all_claimed.extend(batch_claimed);
    }

    assert_eq!(
        all_claimed.len(),
        100,
        "all 100 commands must be drained; got {}",
        all_claimed.len()
    );

    // No duplicate command IDs.
    let unique: std::collections::HashSet<&String> = all_claimed.iter().collect();
    assert_eq!(
        unique.len(),
        100,
        "no command must be claimed twice; got {} unique from {}",
        unique.len(),
        all_claimed.len()
    );
}

// ─── Server restart recovery test ────────────────────────────────────────────

/// Insert a session and a running command, then run recover_stale_commands.
/// The running command must be reset to 'queued'.
#[test]
fn server_restart_recovery_resets_stale_running_commands() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();

    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    // Simulate a command that was running when the server crashed.
    store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                         started_at, worker_id, queued_at)
                 VALUES ('crash-cmd', 's1', 'exec workflow', '[]', 'running', '/logs/crash',
                         '2020-01-01T00:00:00Z', 'dead-worker', '2020-01-01T00:00:00Z')",
                [],
            )
            .map_err(awman::data::error::DataError::from)
        })
        .unwrap();

    // recover_stale_commands with a 1-second timeout recovers this old command.
    let recovered = store.recover_stale_commands(1).unwrap();
    assert!(
        recovered.contains(&"crash-cmd".to_string()),
        "crash-cmd must be recovered; got {recovered:?}"
    );

    let cmd = store.get_command("crash-cmd").unwrap().unwrap();
    assert_eq!(cmd.status, "queued");
}

// ─── Worker count config test ─────────────────────────────────────────────────

/// GlobalConfig::workers() defaults to 2 when not set.
#[test]
fn global_config_workers_defaults_to_two() {
    use awman::data::config::global::GlobalConfig;
    let cfg = GlobalConfig::default();
    assert_eq!(cfg.workers(), 2, "default worker count must be 2");
}

/// Setting workers: 0 returns 0 from workers().
#[test]
fn global_config_workers_zero_means_zero() {
    use awman::data::config::global::GlobalConfig;
    let cfg = GlobalConfig {
        workers: Some(0),
        ..Default::default()
    };
    assert_eq!(
        cfg.workers(),
        0,
        "workers: 0 config must result in workers() == 0"
    );
}

/// workers: 4 returns 4.
#[test]
fn global_config_workers_explicit_value_is_respected() {
    use awman::data::config::global::GlobalConfig;
    let cfg = GlobalConfig {
        workers: Some(4),
        ..Default::default()
    };
    assert_eq!(cfg.workers(), 4);
}

// ─── HTTP integration tests ───────────────────────────────────────────────────

/// POST /v1/commands on a ready session enqueues the command with status='queued'.
/// The DB row must NOT have status='pending' or 'running'.
#[tokio::test]
async fn real_network_post_commands_enqueues_with_status_queued() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "sess-enqueue", "/work");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "sess-enqueue")
        .json(&serde_json::json!({
            "subcommand": "exec prompt",
            "args": ["--prompt", "hello"]
        }))
        .send()
        .await
        .expect("POST /v1/commands");

    assert_eq!(resp.status().as_u16(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    let command_id = body["command_id"].as_str().expect("command_id missing");

    let cmd_record = state.store.get_command(command_id).unwrap().unwrap();
    assert_eq!(
        cmd_record.status, "queued",
        "command inserted by POST must have status='queued', not '{}' or 'pending'",
        cmd_record.status
    );
    assert!(
        cmd_record.queued_at.is_some(),
        "queued_at must be set on enqueue"
    );
    assert!(
        cmd_record.worker_id.is_none(),
        "worker_id must be NULL immediately after enqueue"
    );

    server.abort();
}

/// POST /v1/commands twice to the same session returns 202 both times.
/// The second command is queued behind the first (no 403/409 blocking).
#[tokio::test]
async fn real_network_post_commands_no_longer_blocks_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "sess-double", "/work");

    let client = reqwest::Client::new();
    let post_cmd = |args: &'static str| {
        let client = client.clone();
        let addr_str = format!("http://{addr}/v1/commands");
        async move {
            client
                .post(&addr_str)
                .header("x-awman-session", "sess-double")
                .json(&serde_json::json!({
                    "subcommand": "exec prompt",
                    "args": [args]
                }))
                .send()
                .await
                .expect("POST /v1/commands")
                .status()
                .as_u16()
        }
    };

    let (s1, s2) = tokio::join!(post_cmd("first"), post_cmd("second"));

    assert_eq!(s1, 202, "first command must return 202; got {s1}");
    assert_eq!(
        s2, 202,
        "second command to same session must also return 202 (queue, not block); got {s2}"
    );

    // Both must be in the DB as 'queued'.
    let cmds = state
        .store
        .list_commands_for_session("sess-double", 10)
        .unwrap();
    assert_eq!(cmds.len(), 2, "both commands must be in the DB");
    for c in &cmds {
        assert_eq!(c.status, "queued", "both commands must be 'queued'");
    }

    server.abort();
}

/// GET /v1/commands/{id}/status for a queued command returns the expected
/// response shape including queued_at, queue_position, worker_id (null), result (null).
#[tokio::test]
async fn real_network_command_status_response_shape_is_correct() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "shape-sess", "/work");

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "shape-sess")
        .json(&serde_json::json!({
            "subcommand": "exec prompt",
            "args": ["test"]
        }))
        .send()
        .await
        .unwrap();
    let cid = create_resp.json::<serde_json::Value>().await.unwrap();
    let command_id = cid["command_id"].as_str().unwrap().to_string();

    let status_resp = client
        .get(format!("http://{addr}/v1/commands/{command_id}/status"))
        .send()
        .await
        .expect("GET /v1/commands/{id}/status");
    assert_eq!(status_resp.status().as_u16(), 200);
    let body: serde_json::Value = status_resp.json().await.unwrap();

    assert_eq!(
        body["status"].as_str(),
        Some("queued"),
        "newly enqueued command must have status=queued; got {body}"
    );
    assert!(
        body.get("queued_at").is_some() && !body["queued_at"].is_null(),
        "response must include non-null queued_at; got {body}"
    );
    assert!(
        body.get("queue_position").is_some(),
        "response must include queue_position field; got {body}"
    );
    assert_eq!(
        body["queue_position"].as_i64(),
        Some(0),
        "first queued command must be at position 0; got {body}"
    );
    // worker_id and result are omitted (skip_serializing_if = "Option::is_none")
    // when the command is queued — not yet claimed by a worker.
    // Verify they are absent (not yet set), which is correct behavior.
    assert!(
        body.get("worker_id").is_none() || body["worker_id"].is_null(),
        "queued command must have no worker_id; got {body}"
    );
    assert!(
        body.get("result").is_none() || body["result"].is_null(),
        "queued command must have no result; got {body}"
    );

    server.abort();
}

/// GET /v1/commands/{id}/status returns queue_position 0, 1, 2 for three
/// queued commands in the same session.
#[tokio::test]
async fn real_network_queue_position_is_zero_indexed_per_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "pos-sess", "/work");

    let client = reqwest::Client::new();
    let mut command_ids = Vec::new();

    for i in 0..3u32 {
        // Small delay to ensure FIFO order by queued_at.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let resp = client
            .post(format!("http://{addr}/v1/commands"))
            .header("x-awman-session", "pos-sess")
            .json(&serde_json::json!({
                "subcommand": "exec prompt",
                "args": [format!("cmd-{i}")]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().await.unwrap();
        command_ids.push(body["command_id"].as_str().unwrap().to_string());
    }

    for (expected_pos, cmd_id) in command_ids.iter().enumerate() {
        let status_resp = client
            .get(format!("http://{addr}/v1/commands/{cmd_id}/status"))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = status_resp.json().await.unwrap();
        let pos = body["queue_position"].as_i64();
        assert_eq!(
            pos,
            Some(expected_pos as i64),
            "command {expected_pos} must be at queue_position={expected_pos}; got {body}"
        );
    }

    server.abort();
}

/// GET /v1/sessions/{id}/queue — returns correct queue state.
#[tokio::test]
async fn real_network_queue_status_endpoint_returns_correct_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let sess_id = "queue-state-sess";
    insert_ready_session(&state.store, sess_id, "/work");

    // Manually set up 3 commands: 1 done, 1 running, 1 queued.
    let now = chrono::Utc::now().to_rfc3339();
    state
        .store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                     queued_at, started_at, finished_at, exit_code)
             VALUES ('cmd-done', ?1, 'exec prompt', '[]', 'done', '/l',
                     ?2, ?2, ?2, 0)",
                rusqlite::params![sess_id, now],
            )
            .map_err(awman::data::error::DataError::from)
        })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let now2 = chrono::Utc::now().to_rfc3339();
    state
        .store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                     queued_at, started_at, worker_id)
             VALUES ('cmd-running', ?1, 'exec prompt', '[]', 'running', '/l',
                     ?2, ?2, 'worker-x')",
                rusqlite::params![sess_id, now2],
            )
            .map_err(awman::data::error::DataError::from)
        })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let now3 = chrono::Utc::now().to_rfc3339();
    state
        .store
        .with_conn(|conn| {
            conn.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at)
             VALUES ('cmd-queued', ?1, 'exec prompt', '[]', 'queued', '/l', ?2)",
            rusqlite::params![sess_id, now3],
        ).map_err(awman::data::error::DataError::from)
        })
        .unwrap();

    let resp = reqwest::get(format!("http://{addr}/v1/sessions/{sess_id}/queue"))
        .await
        .expect("GET /v1/sessions/{id}/queue");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(
        body["session_id"].as_str(),
        Some(sess_id),
        "response must echo session_id"
    );
    assert_eq!(
        body["queue_depth"].as_i64(),
        Some(1),
        "queue_depth must be 1 (one queued command); got {body}"
    );
    assert!(
        !body["running"].is_null(),
        "running must be non-null when a command is running; got {body}"
    );
    assert_eq!(
        body["running"]["command_id"].as_str(),
        Some("cmd-running"),
        "running must identify the running command; got {body}"
    );

    let queued_arr = body["queued"].as_array().expect("queued must be array");
    assert_eq!(
        queued_arr.len(),
        1,
        "exactly 1 queued command; got {queued_arr:?}"
    );
    assert_eq!(queued_arr[0]["command_id"].as_str(), Some("cmd-queued"));
    assert_eq!(queued_arr[0]["position"].as_i64(), Some(0));

    let completed_arr = body["recent_completed"]
        .as_array()
        .expect("recent_completed must be array");
    assert_eq!(
        completed_arr.len(),
        1,
        "exactly 1 completed command; got {completed_arr:?}"
    );
    assert_eq!(completed_arr[0]["command_id"].as_str(), Some("cmd-done"));

    server.abort();
}

/// GET /v1/sessions/{id}/queue for a session with no commands returns an
/// empty-queue response.
#[tokio::test]
async fn real_network_queue_status_empty_session_returns_empty_response() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "empty-sess", "/work");

    let resp = reqwest::get(format!("http://{addr}/v1/sessions/empty-sess/queue"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body["queue_depth"].as_i64(), Some(0));
    assert!(body["running"].is_null());
    assert_eq!(body["queued"].as_array().unwrap().len(), 0);
    assert_eq!(body["recent_completed"].as_array().unwrap().len(), 0);

    server.abort();
}

/// GET /v1/sessions/{id}/queue for an unknown session returns 404.
#[tokio::test]
async fn real_network_queue_status_unknown_session_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::get(format!("http://{addr}/v1/sessions/no-such-sess/queue"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    server.abort();
}

// ─── Session lifecycle: DELETE ────────────────────────────────────────────────

/// DELETE a session with no commands — responds 200, session is 'closed'.
#[tokio::test]
async fn real_network_delete_session_with_empty_queue_closes_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "del-empty", "/work");

    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://{addr}/v1/sessions/del-empty"))
        .send()
        .await
        .expect("DELETE /v1/sessions/del-empty");

    assert_eq!(
        resp.status().as_u16(),
        200,
        "empty-queue delete must return 200"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["status"].as_str(),
        Some("closed"),
        "session must be 'closed' after delete; got {body}"
    );

    let rec = state.store.get_session("del-empty").unwrap().unwrap();
    assert_eq!(rec.status, "closed");

    server.abort();
}

/// DELETE a session with 3 queued commands (no running command) — all 3 are
/// cancelled, session closes immediately, HTTP 200.
#[tokio::test]
async fn real_network_delete_session_with_queued_commands_cancels_all() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "del-queued", "/work");

    // Enqueue 3 commands directly.
    for i in 0..3u32 {
        state
            .store
            .enqueue_command(
                &format!("qcmd-{i}"),
                "del-queued",
                "exec prompt",
                "[]",
                "/l",
            )
            .unwrap();
    }

    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://{addr}/v1/sessions/del-queued"))
        .send()
        .await
        .expect("DELETE /v1/sessions/del-queued");

    assert_eq!(
        resp.status().as_u16(),
        200,
        "delete with only queued commands must return 200; got {}",
        resp.status()
    );

    // All 3 commands must be cancelled.
    for i in 0..3u32 {
        let cmd = state
            .store
            .get_command(&format!("qcmd-{i}"))
            .unwrap()
            .unwrap();
        assert_eq!(
            cmd.status, "cancelled",
            "qcmd-{i} must be 'cancelled'; got '{}'",
            cmd.status
        );
    }

    // Session must be closed.
    let rec = state.store.get_session("del-queued").unwrap().unwrap();
    assert_eq!(
        rec.status, "closed",
        "session must be 'closed' after delete; got '{}'",
        rec.status
    );

    server.abort();
}

/// DELETE a session that has a running command — returns 202 with
/// running_command_id, session enters 'closing' state.
#[tokio::test]
async fn real_network_delete_session_with_running_command_returns_202() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "del-running", "/work");

    let now = chrono::Utc::now().to_rfc3339();
    // Insert a running command manually.
    state
        .store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                     queued_at, started_at, worker_id)
             VALUES ('running-cmd', 'del-running', 'exec prompt', '[]', 'running', '/l',
                     ?1, ?1, 'active-worker')",
                rusqlite::params![now],
            )
            .map_err(awman::data::error::DataError::from)
        })
        .unwrap();

    // Also enqueue a second command that should get cancelled.
    std::thread::sleep(std::time::Duration::from_millis(2));
    state
        .store
        .enqueue_command("queued-cmd", "del-running", "exec prompt", "[]", "/l")
        .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://{addr}/v1/sessions/del-running"))
        .send()
        .await
        .expect("DELETE /v1/sessions/del-running");

    assert_eq!(
        resp.status().as_u16(),
        202,
        "delete with running command must return 202; got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["status"].as_str(),
        Some("closing"),
        "session status must be 'closing'; got {body}"
    );
    assert_eq!(
        body["running_command_id"].as_str(),
        Some("running-cmd"),
        "running_command_id must be 'running-cmd'; got {body}"
    );

    // The queued command must be cancelled.
    let queued = state.store.get_command("queued-cmd").unwrap().unwrap();
    assert_eq!(
        queued.status, "cancelled",
        "queued-cmd must be cancelled when session is being deleted"
    );

    // Session must be 'closing'.
    let rec = state.store.get_session("del-running").unwrap().unwrap();
    assert_eq!(rec.status, "closing");

    server.abort();
}

/// POST /v1/commands on a 'closing' session returns HTTP 409.
#[tokio::test]
async fn real_network_post_commands_rejected_on_closing_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    // Insert a session and manually set it to 'closing'.
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .store
        .insert_session_full("closing-sess", "/work", &ts, "ready", "local", None)
        .unwrap();
    state
        .store
        .update_session_status("closing-sess", "closing")
        .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "closing-sess")
        .json(&serde_json::json!({
            "subcommand": "exec prompt",
            "args": []
        }))
        .send()
        .await
        .expect("POST /v1/commands to closing session");

    assert_eq!(
        resp.status().as_u16(),
        409,
        "POST to closing session must return 409; got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("closing"),
        "409 body must mention 'closing'; got {body}"
    );

    server.abort();
}

/// DELETE an already-closing session returns 200 with current state, no error.
#[tokio::test]
async fn real_network_double_delete_returns_200_with_current_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    // Create a session and manually put it in 'closing'.
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .store
        .insert_session_full("double-del-sess", "/work", &ts, "ready", "local", None)
        .unwrap();
    state
        .store
        .update_session_status("double-del-sess", "closing")
        .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://{addr}/v1/sessions/double-del-sess"))
        .send()
        .await
        .expect("DELETE already-closing session");

    // Must not be an error — returns 200 (already closing, idempotent).
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        status == 200 || status == 202,
        "double-delete must return 200 or 202; got {status}; body={body}"
    );
    assert!(
        body.get("error").is_none(),
        "double-delete must not return an error; got {body}"
    );

    server.abort();
}

/// DELETE an already-closed session returns 200.
#[tokio::test]
async fn real_network_delete_already_closed_session_returns_200() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let ts = chrono::Utc::now().to_rfc3339();
    state
        .store
        .insert_session_full("already-closed", "/work", &ts, "ready", "local", None)
        .unwrap();
    state.store.close_session("already-closed", &ts).unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://{addr}/v1/sessions/already-closed"))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        200,
        "delete of closed session must return 200"
    );

    server.abort();
}

// ─── Local and remote session creation tests ─────────────────────────────────

/// POST /v1/sessions with type=local and a valid workdir creates the session.
#[tokio::test]
async fn real_network_local_session_creation_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path().canonicalize().unwrap();

    // App state with the workdir in allowlist.
    let paths = ApiPaths::from_root(tmp.path());
    paths.ensure_root().expect("ensure_root");
    let store = SqliteSessionStore::open(paths.root()).expect("open sqlite");
    let auth_paths = AuthPathResolver::at_home(tmp.path());
    let runtime = Arc::new(ContainerRuntime::docker());
    let git_engine = Arc::new(GitEngine::new());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, paths.clone()));
    let workflow_state_store = Arc::new(EngineWorkflowStateStore::at_git_root(paths.root()));
    let engines = Engines {
        runtime,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
    };
    let state = Arc::new(AppState {
        store: Arc::new(store),
        paths,
        workdirs: vec![workdir_path.clone()],
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    });

    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "local",
            "workdir": workdir_path.display().to_string()
        }))
        .send()
        .await
        .expect("POST /v1/sessions");

    assert_eq!(
        resp.status().as_u16(),
        202,
        "local session creation must return 202"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    let session_id = body["session_id"].as_str().expect("session_id missing");

    // Session must exist in the DB.
    let rec = state.store.get_session(session_id).unwrap();
    assert!(rec.is_some(), "session must be persisted in the DB");
    let rec = rec.unwrap();
    assert_eq!(rec.session_type, "local");

    server.abort();
}

/// POST /v1/sessions with type=remote and an invalid URL (file:// scheme) is rejected.
#[tokio::test]
async fn real_network_remote_session_rejects_file_url_scheme() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "remote",
            "repo_url": "file:///etc/passwd",
            "branch": "main"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "file:// URL must be rejected with 400"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap_or("").contains("scheme"),
        "error must mention scheme restriction; got {body}"
    );

    server.abort();
}

/// POST /v1/sessions with type=remote and missing repo_url returns 400.
#[tokio::test]
async fn real_network_remote_session_missing_repo_url_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "remote",
            "branch": "main"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);

    server.abort();
}

// ─── Workflow state endpoint test ─────────────────────────────────────────────

/// After manually writing a workflow state file, GET /v1/workflows/{command_id}
/// returns the correct JSON.
#[tokio::test]
async fn real_network_workflow_state_endpoint_returns_correct_json() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let sess_id = "wf-state-sess";
    let cmd_id = "wf-state-cmd";
    insert_ready_session(&state.store, sess_id, "/work");
    state
        .store
        .enqueue_command(cmd_id, sess_id, "exec workflow", r#"["wf.toml"]"#, "/l")
        .unwrap();

    // Write workflow state to the expected path.
    let wf_path = state.paths.command_workflow_state_path(sess_id, cmd_id);
    tokio::fs::create_dir_all(wf_path.parent().unwrap())
        .await
        .unwrap();
    let wf_state = serde_json::json!({
        "schema_version": 2,
        "workflow_name": "test-workflow",
        "workflow_hash": "abc123",
        "step_states": { "analyze": "Succeeded" },
        "completed_steps": ["analyze"],
        "current_step_index": null,
        "started_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:01:00Z"
    });
    tokio::fs::write(&wf_path, serde_json::to_string(&wf_state).unwrap())
        .await
        .unwrap();

    let resp = reqwest::get(format!("http://{addr}/v1/workflows/{cmd_id}"))
        .await
        .expect("GET /v1/workflows/{command_id}");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(
        body["workflow_name"].as_str(),
        Some("test-workflow"),
        "workflow_name must match; got {body}"
    );
    assert_eq!(
        body["workflow_hash"].as_str(),
        Some("abc123"),
        "workflow_hash must match; got {body}"
    );

    server.abort();
}

/// GET /v1/workflows/{command_id} returns 404 when no workflow state exists.
#[tokio::test]
async fn real_network_workflow_state_404_when_no_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "wf-404-sess", "/work");
    state
        .store
        .enqueue_command("wf-404-cmd", "wf-404-sess", "exec workflow", "[]", "/l")
        .unwrap();

    let resp = reqwest::get(format!("http://{addr}/v1/workflows/wf-404-cmd"))
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "workflow state must 404 when no state file exists"
    );

    server.abort();
}

// ─── workflow_state_to_view_state unit tests ──────────────────────────────────

/// workflow_state_to_view_state maps setup/main/teardown steps correctly.
#[test]
fn workflow_state_to_view_state_maps_all_phases() {
    use awman::data::workflow_definition::WorkflowStep;
    use awman::data::workflow_state::{PhaseStepState, PhaseStepStatus, WorkflowState};
    use awman::frontend::tui::workflow_view::workflow_state_to_view_state;

    fn ws(name: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            name: name.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            prompt_template: String::new(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        }
    }

    let steps = vec![ws("analyze", &[]), ws("implement", &["analyze"])];
    let mut state = WorkflowState::new("test-wf".into(), &steps, "hash".into(), None);

    // Add setup and teardown pseudo-steps.
    state.setup_step_states = vec![
        PhaseStepState {
            description: "clone_repo".to_string(),
            status: PhaseStepStatus::Succeeded,
        },
        PhaseStepState {
            description: "install_deps".to_string(),
            status: PhaseStepStatus::Running,
        },
    ];
    state.teardown_step_states = vec![PhaseStepState {
        description: "commit_changes".to_string(),
        status: PhaseStepStatus::Pending,
    }];

    let view = workflow_state_to_view_state(&state);

    // Total steps = 2 setup + 2 main + 1 teardown = 5.
    assert_eq!(
        view.steps.len(),
        5,
        "must have 5 total steps (2 setup + 2 main + 1 teardown); got {}",
        view.steps.len()
    );

    // Setup steps come first with [setup] prefix.
    assert!(
        view.steps[0].name.starts_with("[setup]"),
        "first step must be a setup step; got '{}'",
        view.steps[0].name
    );
    assert!(
        view.steps[0].name.contains("clone_repo"),
        "first setup step must be clone_repo; got '{}'",
        view.steps[0].name
    );
    assert_eq!(
        view.steps[0].status, "done",
        "succeeded setup step must have status='done'"
    );

    assert!(
        view.steps[1].name.contains("install_deps"),
        "second setup step must be install_deps; got '{}'",
        view.steps[1].name
    );
    assert_eq!(
        view.steps[1].status, "running",
        "running setup step must have status='running'"
    );

    // Main steps follow.
    assert_eq!(
        view.steps[2].name, "analyze",
        "third step must be 'analyze'; got '{}'",
        view.steps[2].name
    );
    assert_eq!(
        view.steps[3].name, "implement",
        "fourth step must be 'implement'; got '{}'",
        view.steps[3].name
    );

    // Teardown comes last with [teardown] prefix.
    assert!(
        view.steps[4].name.starts_with("[teardown]"),
        "last step must be a teardown step; got '{}'",
        view.steps[4].name
    );
    assert!(
        view.steps[4].name.contains("commit_changes"),
        "teardown step must be commit_changes; got '{}'",
        view.steps[4].name
    );
    assert_eq!(
        view.steps[4].status, "pending",
        "pending teardown step must have status='pending'"
    );
}

/// workflow_state_to_view_state with no setup/teardown maps only main steps.
#[test]
fn workflow_state_to_view_state_no_phase_steps() {
    use awman::data::workflow_definition::WorkflowStep;
    use awman::data::workflow_state::WorkflowState;
    use awman::frontend::tui::workflow_view::workflow_state_to_view_state;

    let steps = vec![WorkflowStep {
        name: "only-step".into(),
        depends_on: vec![],
        prompt_template: String::new(),
        agent: Some("claude".into()),
        model: None,
        overlays: None,
        abort_on_failure: false,
    }];
    let state = WorkflowState::new("minimal-wf".into(), &steps, "h".into(), None);
    let view = workflow_state_to_view_state(&state);

    assert_eq!(view.steps.len(), 1);
    assert_eq!(view.steps[0].name, "only-step");
    assert_eq!(view.steps[0].status, "pending");
    assert_eq!(view.steps[0].agent.as_deref(), Some("claude"));
}

/// workflow_state_to_view_state with a running step sets current_step correctly.
#[test]
fn workflow_state_to_view_state_current_step_index() {
    use awman::data::workflow_definition::WorkflowStep;
    use awman::data::workflow_state::{StepState, WorkflowState};
    use awman::frontend::tui::workflow_view::workflow_state_to_view_state;

    let steps = vec![
        WorkflowStep {
            name: "alpha".into(),
            depends_on: vec![],
            prompt_template: String::new(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        },
        WorkflowStep {
            name: "beta".into(),
            depends_on: vec!["alpha".into()],
            prompt_template: String::new(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        },
    ];
    let mut state = WorkflowState::new("wf".into(), &steps, "h".into(), None);
    state.set_status("alpha", StepState::Succeeded);
    state.set_status("beta", StepState::Running { container_id: None });
    state.current_step_index = Some(1); // beta is at index 1

    let view = workflow_state_to_view_state(&state);
    assert_eq!(
        view.steps[0].status, "done",
        "alpha must be 'done'; got '{}'",
        view.steps[0].status
    );
    assert_eq!(
        view.steps[1].status, "running",
        "beta must be 'running'; got '{}'",
        view.steps[1].status
    );
    assert_eq!(
        view.current_step.as_deref(),
        Some("beta"),
        "current_step must be 'beta'; got {:?}",
        view.current_step
    );
}

/// workflow_state_to_view_state with all phase statuses maps them correctly.
#[test]
fn workflow_state_to_view_state_phase_step_statuses() {
    use awman::data::workflow_definition::WorkflowStep;
    use awman::data::workflow_state::{PhaseStepState, PhaseStepStatus, WorkflowState};
    use awman::frontend::tui::workflow_view::workflow_state_to_view_state;

    let steps = vec![WorkflowStep {
        name: "main-step".into(),
        depends_on: vec![],
        prompt_template: String::new(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    }];
    let mut state = WorkflowState::new("wf".into(), &steps, "h".into(), None);
    state.setup_step_states = vec![
        PhaseStepState {
            description: "s-pending".into(),
            status: PhaseStepStatus::Pending,
        },
        PhaseStepState {
            description: "s-running".into(),
            status: PhaseStepStatus::Running,
        },
        PhaseStepState {
            description: "s-succeeded".into(),
            status: PhaseStepStatus::Succeeded,
        },
        PhaseStepState {
            description: "s-failed".into(),
            status: PhaseStepStatus::Failed {
                error: "oops".into(),
            },
        },
    ];

    let view = workflow_state_to_view_state(&state);

    // 4 setup + 1 main = 5 steps.
    assert_eq!(view.steps.len(), 5);
    assert_eq!(view.steps[0].status, "pending");
    assert_eq!(view.steps[1].status, "running");
    assert_eq!(view.steps[2].status, "done");
    assert_eq!(view.steps[3].status, "error");
}

// ─── WorkerId unit tests ───────────────────────────────────────────────────────

#[test]
fn worker_id_new_generates_unique_ids() {
    use awman::data::fs::api_db::WorkerId;
    let a = WorkerId::new();
    let b = WorkerId::new();
    assert_ne!(a.as_str(), b.as_str(), "two WorkerIds must be distinct");
}

#[test]
fn worker_id_serializes_and_deserializes() {
    use awman::data::fs::api_db::WorkerId;
    let id = WorkerId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: WorkerId = serde_json::from_str(&json).unwrap();
    assert_eq!(id.as_str(), back.as_str());
}

// ─── complete_command unit test ────────────────────────────────────────────────

#[test]
fn complete_command_sets_status_and_result() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();
    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/l")
        .unwrap();
    store.claim_next_command("w1").unwrap();

    let result_json = r#"{"exit_code":0}"#;
    store
        .complete_command("c1", "done", Some(0), Some(result_json))
        .unwrap();

    let cmd = store.get_command("c1").unwrap().unwrap();
    assert_eq!(cmd.status, "done");
    assert_eq!(cmd.exit_code, Some(0));
    assert!(cmd.finished_at.is_some());
    assert_eq!(cmd.result.as_deref(), Some(result_json));
}

#[test]
fn complete_command_error_path() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();
    store
        .enqueue_command("c1", "s1", "exec workflow", "[]", "/l")
        .unwrap();
    store.claim_next_command("w1").unwrap();

    let result_json = r#"{"exit_code":1,"error":"workflow file not found"}"#;
    store
        .complete_command("c1", "error", Some(1), Some(result_json))
        .unwrap();

    let cmd = store.get_command("c1").unwrap().unwrap();
    assert_eq!(cmd.status, "error");
    assert_eq!(cmd.exit_code, Some(1));
    assert!(cmd.result.as_deref().unwrap().contains("not found"));
}

// ─── Session status transition tests ─────────────────────────────────────────

#[test]
fn update_session_status_sets_arbitrary_status() {
    let (_tmp, store) = make_store();
    store
        .insert_session("s1", "/wd", &chrono::Utc::now().to_rfc3339())
        .unwrap();
    let ok = store.update_session_status("s1", "closing").unwrap();
    assert!(ok);
    let rec = store.get_session("s1").unwrap().unwrap();
    assert_eq!(rec.status, "closing");
}

#[test]
fn close_session_force_closes_from_closing_state() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store.insert_session("s1", "/wd", &ts).unwrap();
    store.update_session_status("s1", "closing").unwrap();

    let closed_at = chrono::Utc::now().to_rfc3339();
    let ok = store.close_session_force("s1", &closed_at).unwrap();
    assert!(ok);

    let rec = store.get_session("s1").unwrap().unwrap();
    assert_eq!(rec.status, "closed");
    assert!(rec.closed_at.is_some());
}

#[test]
fn close_session_force_is_idempotent_for_already_closed() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store.insert_session("s1", "/wd", &ts).unwrap();
    store.close_session_force("s1", &ts).unwrap();
    // Second close must not change anything.
    let ok = store.close_session_force("s1", &ts).unwrap();
    assert!(
        !ok,
        "close_session_force on already-closed must return false"
    );
}

// ─── Regression tests for review-cycle fixes ─────────────────────────────────

/// GET /v1/sessions/{id}/queue must report queued commands oldest-first
/// (position 0 = next to run). Previously the handler iterated the
/// `queued_at DESC` list result and assigned position 0 to the newest.
#[tokio::test]
async fn real_network_queue_status_position_is_oldest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let sess_id = "queue-position-order";
    insert_ready_session(&state.store, sess_id, "/work");

    // Insert 3 queued commands at distinct timestamps.
    let t1 = chrono::Utc::now().to_rfc3339();
    state
        .store
        .with_conn(|conn| {
            conn.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at)
             VALUES ('oldest', ?1, 'exec prompt', '[]', 'queued', '/l', ?2)",
            rusqlite::params![sess_id, t1],
        ).map_err(awman::data::error::DataError::from)
        })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t2 = chrono::Utc::now().to_rfc3339();
    state
        .store
        .with_conn(|conn| {
            conn.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at)
             VALUES ('middle', ?1, 'exec prompt', '[]', 'queued', '/l', ?2)",
            rusqlite::params![sess_id, t2],
        ).map_err(awman::data::error::DataError::from)
        })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t3 = chrono::Utc::now().to_rfc3339();
    state
        .store
        .with_conn(|conn| {
            conn.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at)
             VALUES ('newest', ?1, 'exec prompt', '[]', 'queued', '/l', ?2)",
            rusqlite::params![sess_id, t3],
        ).map_err(awman::data::error::DataError::from)
        })
        .unwrap();

    let resp = reqwest::get(format!("http://{addr}/v1/sessions/{sess_id}/queue"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    let queued = body["queued"].as_array().expect("queued must be array");
    assert_eq!(
        queued.len(),
        3,
        "all 3 queued commands must appear; got {body}"
    );

    assert_eq!(
        queued[0]["command_id"].as_str(),
        Some("oldest"),
        "position 0 must be the oldest-queued command (next to run); got {body}"
    );
    assert_eq!(queued[0]["position"].as_i64(), Some(0));
    assert_eq!(queued[1]["command_id"].as_str(), Some("middle"));
    assert_eq!(queued[1]["position"].as_i64(), Some(1));
    assert_eq!(queued[2]["command_id"].as_str(), Some("newest"));
    assert_eq!(queued[2]["position"].as_i64(), Some(2));

    server.abort();
}

/// `recent_completed` must be ordered by `finished_at DESC`, not by
/// `queued_at` (which is what the underlying query returns).
#[tokio::test]
async fn real_network_queue_status_recent_completed_orders_by_finished_at() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let sess_id = "recent-completed-order";
    insert_ready_session(&state.store, sess_id, "/work");

    // c-old: queued earlier, but finished LATER (slow command).
    // c-new: queued later, but finished SOONER (fast command).
    // Expected order in recent_completed: [c-old, c-new] (by finished_at DESC).
    let q_old = chrono::Utc::now().to_rfc3339();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let q_new = chrono::Utc::now().to_rfc3339();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let f_new = chrono::Utc::now().to_rfc3339();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let f_old = chrono::Utc::now().to_rfc3339();

    state
        .store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                     queued_at, started_at, finished_at, exit_code)
             VALUES ('c-old', ?1, 'exec prompt', '[]', 'done', '/l', ?2, ?2, ?3, 0)",
                rusqlite::params![sess_id, q_old, f_old],
            )
            .map_err(awman::data::error::DataError::from)
        })
        .unwrap();
    state
        .store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                     queued_at, started_at, finished_at, exit_code)
             VALUES ('c-new', ?1, 'exec prompt', '[]', 'done', '/l', ?2, ?2, ?3, 0)",
                rusqlite::params![sess_id, q_new, f_new],
            )
            .map_err(awman::data::error::DataError::from)
        })
        .unwrap();

    let resp = reqwest::get(format!("http://{addr}/v1/sessions/{sess_id}/queue"))
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let completed = body["recent_completed"].as_array().unwrap();
    assert_eq!(completed.len(), 2);
    assert_eq!(
        completed[0]["command_id"].as_str(),
        Some("c-old"),
        "command with later finished_at must appear first; got {body}"
    );
    assert_eq!(completed[1]["command_id"].as_str(), Some("c-new"));

    server.abort();
}

/// In-memory Session for a remote API session must report `is_remote() == true`.
/// Direct unit test on the type — covers the restore path and acts as a
/// regression guard for the fix in `run_session_setup`.
#[test]
fn session_set_session_type_remote_is_observable() {
    use awman::data::session::{Session, SessionOpenOptions, SessionType, StaticGitRootResolver};

    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = awman::data::config::env::EnvSnapshot::with_overrides([(
        awman::data::config::env::AWMAN_CONFIG_HOME,
        home.path().to_str().unwrap(),
    )]);

    let resolver = StaticGitRootResolver::new(tmp.path());
    let mut session = Session::open(
        tmp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();

    assert!(!session.session_type().is_remote());

    let cloned = tmp.path().to_path_buf();
    session.set_session_type(SessionType::Remote {
        repo_url: "https://example.invalid/repo.git".to_string(),
        branch: "main".to_string(),
        cloned_path: cloned.clone(),
    });

    assert!(session.session_type().is_remote());
    assert_eq!(session.session_type().cloned_path(), Some(cloned.as_path()));
    assert_eq!(session.working_dir(), cloned.as_path());
}

/// `recover_stale_commands(0)` (the startup recovery threshold) recovers
/// every command in `running` state, regardless of how recently it started.
/// Previously the threshold was 300s, leaving freshly-started commands
/// stranded for 5 minutes after a server restart.
#[test]
fn recover_stale_commands_zero_threshold_recovers_all_running() {
    let (_tmp, store) = make_store();
    let ts = chrono::Utc::now().to_rfc3339();
    store
        .insert_session_full("s1", "/wd", &ts, "ready", "local", None)
        .unwrap();

    // A command started "just now" — well within any non-zero stale window.
    store
        .enqueue_command("c-fresh", "s1", "exec prompt", "[]", "/l")
        .unwrap();
    // Claim it so it transitions to running with a recent started_at.
    let claimed = store.claim_next_command("worker-x").unwrap();
    assert!(claimed.is_some(), "fresh command must be claimable");

    // Sanity: with a 300s threshold it would NOT be recovered.
    let with_old_threshold = store.recover_stale_commands(300).unwrap();
    assert!(
        with_old_threshold.is_empty(),
        "300s threshold must not recover a freshly-started command"
    );
    let cmd = store.get_command("c-fresh").unwrap().unwrap();
    assert_eq!(cmd.status, "running");

    // With a 0s threshold (startup recovery), the command IS recovered.
    let recovered = store.recover_stale_commands(0).unwrap();
    assert_eq!(recovered, vec!["c-fresh".to_string()]);
    let cmd = store.get_command("c-fresh").unwrap().unwrap();
    assert_eq!(cmd.status, "queued");
    assert!(
        cmd.worker_id.is_none(),
        "worker_id must be cleared on recover"
    );
    assert!(
        cmd.started_at.is_none(),
        "started_at must be cleared on recover"
    );
}

/// DELETE /v1/sessions/{id} must set the session to 'closing' BEFORE
/// cancelling queued commands, so that any in-flight POST /v1/commands
/// is rejected with 409 once the gate is closed. This test verifies the
/// gate behavior: after DELETE returns, a fresh POST must be rejected.
#[tokio::test]
async fn real_network_post_after_delete_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let sess_id = "delete-then-post";
    insert_ready_session(&state.store, sess_id, "/work");

    // DELETE the empty session — closes immediately.
    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://{addr}/v1/sessions/{sess_id}"))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "DELETE must succeed; got {}",
        resp.status()
    );

    // POST a command to the now-closed session — must be rejected (409 or 404).
    let resp = client
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", sess_id)
        .json(&serde_json::json!({
            "subcommand": "exec prompt",
            "args": ["--prompt", "hi"],
        }))
        .send()
        .await
        .unwrap();
    let code = resp.status().as_u16();
    assert!(
        matches!(code, 404 | 409),
        "POST to closed session must be rejected with 404 or 409; got {code}"
    );

    server.abort();
}
