//! The client half of the daemon-env protocol, end to end, against a real
//! daemon (WI 0116 §4a on the WI 0140 transport).
//!
//! `DaemonResolver::gateway_for(GatewayNeed::Running)` is the one call every
//! squad command makes before it talks to the daemon, and the coverage sync
//! hangs off it. The daemon is a real `awman daemon start` process with only
//! the squad feature on; the resolver reaches it over its owner-only local
//! socket (there is no TCP and no bearer key). What the sync did is observed
//! from the daemon's side: the coverage it reports (digest, `last_provided_at`)
//! and the sync report the client returns.
//!
//! The resolver reads the real process environment, so every test takes the
//! same lock and scopes the variables it sets.

#![cfg(unix)]

use std::sync::Arc;

use awman::command::commands::squad::client::DaemonSquadClient;
use awman::command::commands::squad::env_sync;
use awman::command::commands::squad::gateway::{EnvCoverage, TaskGateway};
use awman::command::commands::squad::supervisor::DaemonResolver;
use awman::command::dispatch::catalogue::GatewayNeed;
use awman::data::config::env::Env;
use awman::data::fs::{MountScope, Task, TaskStatus};
use awman::engine::squad::env_state::{coverage_digest, Salt};
use tokio::sync::Mutex as AsyncMutex;

#[path = "helpers/api_process.rs"]
mod api_process;
use api_process::RootDaemon;

static PROCESS_ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

/// Scope the real process environment for one test and restore it on drop.
struct ScopedEnv(Vec<(String, Option<String>)>);

impl ScopedEnv {
    fn set(vars: &[(&str, &str)]) -> Self {
        Self(
            vars.iter()
                .map(|(key, value)| {
                    let previous = std::env::var(key).ok();
                    std::env::set_var(key, value);
                    ((*key).to_string(), previous)
                })
                .collect(),
        )
    }

    fn update(&mut self, key: &str, value: &str) {
        std::env::set_var(key, value);
    }

    fn clear(&mut self, key: &str) {
        self.0.push((key.to_string(), std::env::var(key).ok()));
        std::env::remove_var(key);
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, previous) in self.0.iter().rev() {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A task seeded into the shared database before the daemon boots, so its
/// `env()` names are in `required_env` from the first tick.
fn seed_task(root: &std::path::Path, name: &str, overlays: &[&str]) {
    let db = root.join("data").join("awman.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = awman::data::fs::TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = chrono::Utc::now();
    store
        .create(&Task {
            prerequisites: None,
            id: awman::data::ids::TaskId::new(),
            name: name.into(),
            description: "seeded for the env sync tests".into(),
            repo_scope: root.to_path_buf(),
            mount_scope: MountScope::GitRoot,
            overlays: overlays.iter().map(|s| s.to_string()).collect(),
            interval_secs: 3600,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        })
        .unwrap();
}

async fn boot(root: &std::path::Path, overlays: &[&str]) -> RootDaemon {
    std::fs::write(
        root.join("config.json"),
        r#"{"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}}"#,
    )
    .unwrap();
    seed_task(root, "nightly", overlays);
    RootDaemon::start(root).await
}

fn resolver() -> DaemonResolver {
    DaemonResolver::from_env(&Env::from_process()).unwrap()
}

fn entry<'a>(
    coverage: &'a EnvCoverage,
    name: &str,
) -> Option<&'a awman::engine::squad::env_state::RequiredEnvEntry> {
    coverage.required.iter().find(|e| e.name == name)
}

async fn gateway(resolver: &DaemonResolver, need: GatewayNeed) -> Arc<dyn TaskGateway> {
    resolver
        .gateway_for(need)
        .await
        .expect("the daemon answers")
        .expect("a gateway")
}

/// **The cold call.** A client cannot know `required_env` before the daemon
/// answers, so one pass learns the list and sends the intersection with this
/// shell: the required name it holds goes, the one it lacks is reported absent
/// and nothing the daemon did not ask for is sent.
#[tokio::test]
async fn gateway_for_running_learns_required_env_then_sends_the_intersection() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let daemon = boot(
        tmp.path(),
        &["env(WI0116_SYNC_TOKEN)", "env(WI0116_ABSENT)"],
    )
    .await;
    let mut env = ScopedEnv::set(&[
        ("AWMAN_HOME", tmp.path().to_str().unwrap()),
        ("WI0116_SYNC_TOKEN", "the-value"),
        // Set, but outside `required_env`: it must never be transmitted.
        ("WI0116_NOT_REQUIRED", "never-sent"),
    ]);
    env.clear("WI0116_ABSENT");

    let resolver = resolver();
    let gateway = gateway(&resolver, GatewayNeed::Running).await;
    let coverage = gateway.env_coverage().await.expect("coverage");

    let held = entry(&coverage, "WI0116_SYNC_TOKEN").expect("required");
    let salt = Salt::from_hex(&coverage.salt).expect("salt");
    assert_eq!(
        held.digest.as_deref(),
        Some(coverage_digest(&salt, "WI0116_SYNC_TOKEN", "the-value").as_str()),
        "the daemon holds exactly the value this shell exported"
    );
    assert!(entry(&coverage, "WI0116_ABSENT").unwrap().digest.is_none());
    assert!(
        entry(&coverage, "WI0116_NOT_REQUIRED").is_none(),
        "a name nothing declares is not collected"
    );
    let rendered = serde_json::to_string(&coverage).unwrap();
    assert!(
        !rendered.contains("the-value") && !rendered.contains("never-sent"),
        "{rendered}"
    );
    drop(env);
    daemon.abort();
}

/// Steady state: the digest already matches, so the next sync sends nothing —
/// `last_provided_at` does not move and the report says `unchanged`.
#[tokio::test]
async fn a_matching_digest_sends_nothing_on_the_next_sync() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let daemon = boot(tmp.path(), &["env(WI0116_SYNC_TOKEN)"]).await;
    let _env = ScopedEnv::set(&[
        ("AWMAN_HOME", tmp.path().to_str().unwrap()),
        ("WI0116_SYNC_TOKEN", "the-value"),
    ]);

    let resolver = resolver();
    let gateway = gateway(&resolver, GatewayNeed::Running).await;
    let first = gateway.env_coverage().await.unwrap();
    let provided = entry(&first, "WI0116_SYNC_TOKEN").unwrap().last_provided_at;
    assert!(provided.is_some());

    let report = env_sync::sync_env(gateway.as_ref(), false).await;
    assert_eq!(report.unchanged, vec!["WI0116_SYNC_TOKEN".to_string()]);
    assert!(report.pushed.is_empty(), "{report:?}");
    let second = gateway.env_coverage().await.unwrap();
    assert_eq!(
        entry(&second, "WI0116_SYNC_TOKEN")
            .unwrap()
            .last_provided_at,
        provided,
        "nothing crossed the socket"
    );
    daemon.abort();
}

/// A rotated value is what a push exists for: the digests differ, so the new
/// value goes to the already-running daemon with no restart.
#[tokio::test]
async fn a_rotated_value_is_pushed_to_the_running_daemon() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let daemon = boot(tmp.path(), &["env(WI0116_SYNC_TOKEN)"]).await;
    let mut env = ScopedEnv::set(&[
        ("AWMAN_HOME", tmp.path().to_str().unwrap()),
        ("WI0116_SYNC_TOKEN", "the-old-value"),
    ]);

    let resolver = resolver();
    let gateway = gateway(&resolver, GatewayNeed::Running).await;
    let before = gateway.env_coverage().await.unwrap();
    let old = entry(&before, "WI0116_SYNC_TOKEN").unwrap().digest.clone();

    env.update("WI0116_SYNC_TOKEN", "the-new-value");
    let report = env_sync::sync_env(gateway.as_ref(), false).await;
    assert_eq!(report.pushed, vec!["WI0116_SYNC_TOKEN".to_string()]);
    let after = gateway.env_coverage().await.unwrap();
    let salt = Salt::from_hex(&after.salt).unwrap();
    assert_ne!(entry(&after, "WI0116_SYNC_TOKEN").unwrap().digest, old);
    assert_eq!(
        entry(&after, "WI0116_SYNC_TOKEN")
            .unwrap()
            .digest
            .as_deref(),
        Some(coverage_digest(&salt, "WI0116_SYNC_TOKEN", "the-new-value").as_str())
    );
    drop(env);
    daemon.abort();
}

/// `IfRunning` is the read-only look (`squad` status, the TUI indicator): it
/// builds a client without syncing, so nothing is pushed.
#[tokio::test]
async fn an_if_running_gateway_pushes_nothing() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let daemon = boot(tmp.path(), &["env(WI0116_SYNC_TOKEN)"]).await;
    let _env = ScopedEnv::set(&[
        ("AWMAN_HOME", tmp.path().to_str().unwrap()),
        ("WI0116_SYNC_TOKEN", "the-value"),
    ]);

    let resolver = resolver();
    let gateway = gateway(&resolver, GatewayNeed::IfRunning).await;
    let coverage = gateway.env_coverage().await.unwrap();
    let held = entry(&coverage, "WI0116_SYNC_TOKEN").unwrap();
    assert!(
        held.digest.is_none() && held.last_provided_at.is_none(),
        "looking at a daemon must not move a secret: {held:?}"
    );
    // A bare client built from the discovered endpoint behaves the same.
    let client = resolver.client().unwrap().expect("a running daemon");
    let bare = DaemonSquadClient::new(client);
    assert!(
        entry(&bare.env_coverage().await.unwrap(), "WI0116_SYNC_TOKEN")
            .unwrap()
            .digest
            .is_none()
    );
    daemon.abort();
}

/// No daemon: `Running` refuses with `DaemonNotRunning` (nothing is started on
/// the caller's behalf); `IfRunning` simply has no gateway.
#[tokio::test]
async fn without_a_daemon_running_refuses_and_if_running_answers_none() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set(&[("AWMAN_HOME", tmp.path().to_str().unwrap())]);

    let resolver = resolver();
    assert!(!resolver.daemon_is_running().unwrap());
    match resolver.gateway_for(GatewayNeed::Running).await {
        Err(awman::command::error::CommandError::DaemonNotRunning) => {}
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("Running must refuse without a daemon"),
    }
    assert!(resolver
        .gateway_for(GatewayNeed::IfRunning)
        .await
        .unwrap()
        .is_none());
    assert!(resolver
        .gateway_for(GatewayNeed::None)
        .await
        .unwrap()
        .is_none());
}
