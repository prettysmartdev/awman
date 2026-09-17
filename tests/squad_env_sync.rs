//! WI 0116 §4a — the client half of the daemon-env protocol, end to end.
//!
//! `SquadGatewayResolver::ensure_running` is the one call every squad command
//! makes before it talks to a daemon, and WI 0116 hangs the coverage sync off
//! it. What that sync actually does can only be observed from the daemon's
//! side, so these tests put a **recording HTTP server** where the daemon would
//! be — `wiremock`, the same fake this tree already uses for `HttpCore` — write
//! the endpoint sidecar and pidfile a running daemon would have left, and then
//! assert on the request sequence the supervisor produced.
//!
//! Everything here moves the real process environment (`SquadSupervisor` reads
//! `Env::from_process()`), so it lives in its own test binary and every test
//! takes the same lock.
//!
//! The sidecar is written with `auth_disabled: true`: a supervisor facing a
//! daemon that checks no bearer token mints no key, which keeps these tests
//! from writing an `squad_key.hash` nobody holds.

#![cfg(unix)]

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use awman::command::commands::squad::gateway::{EnvCoverage, TaskGateway};
use awman::command::commands::squad::supervisor::SquadGatewayResolver;
use awman::data::config::env::Env;
use awman::data::fs::daemon_process::{
    DaemonProcess, ServerMeta, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME,
};
use awman::data::fs::{MountScope, SquadPaths, Task, TaskStatus};
use awman::engine::squad::env_state::{coverage_digest, Salt};
use tokio::sync::Mutex as AsyncMutex;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

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

    /// Remember a variable's current value so the test can unset it and have
    /// it restored on drop.
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

/// A live process whose command name contains `awman`, so the pidfile check
/// (`pid_is_awman`) accepts it as a running daemon. The same trick
/// `tests/squad_cli_gateway.rs` uses to stand up an "already running" daemon
/// without a real one.
struct AwmanNamedHolder {
    _dir: tempfile::TempDir,
    child: std::process::Child,
}

impl AwmanNamedHolder {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("holder directory");
        let executable = dir.path().join("awman-env-sync-holder");
        std::fs::copy("/bin/sleep", &executable).expect("copy sleep holder");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let child = Command::new(&executable)
            .arg("120")
            .spawn()
            .expect("holder must start");
        Self { _dir: dir, child }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for AwmanNamedHolder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Write the pidfile and endpoint sidecar a daemon serving `base` would have
/// left behind, so `ensure_running` takes its **already-running** branch.
fn publish_running_daemon(root: &std::path::Path, base: &str, pid: u32) {
    let daemon = DaemonProcess::new(
        SquadPaths::from_root(root.join("squad")).daemon(),
        SQUAD_UNIT_NAME,
        SQUAD_PLIST_LABEL,
    );
    let url = base.strip_prefix("http://").expect("wiremock serves http");
    let (host, port) = url.rsplit_once(':').expect("host:port");
    daemon
        .write_meta(&ServerMeta {
            port: port.parse().expect("port"),
            bind_ip: host.to_string(),
            scheme: "http".into(),
            auth_disabled: true,
        })
        .expect("endpoint sidecar");
    daemon.force_write_pidfile(pid).expect("pidfile");
}

/// One coverage body, as `GET /v1/daemon/env` returns it.
fn coverage_body(salt: &Salt, entries: &[(&str, Option<String>)]) -> serde_json::Value {
    serde_json::json!({
        "salt": salt.to_hex(),
        "persistence": "none",
        "required": entries
            .iter()
            .map(|(name, digest)| serde_json::json!({
                "name": name,
                "required_by": ["nightly"],
                "required_since": "2026-09-09T20:19:54.417577053Z",
                "last_provided_at": null,
                "unmet_since": null,
                "source": digest.as_ref().map(|_| "pushed"),
                "digest": digest,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Every request the fake daemon received, as `(method, path, body)`.
async fn requests(server: &MockServer) -> Vec<(String, String, serde_json::Value)> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|request| {
            let body = serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
            (
                request.method.to_string().to_uppercase(),
                request.url.path().to_string(),
                body,
            )
        })
        .collect()
}

async fn mount_env_routes(server: &MockServer, coverage: serde_json::Value) {
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/v1/daemon/env"))
        .respond_with(ResponseTemplate::new(200).set_body_json(coverage.clone()))
        .mount(server)
        .await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/daemon/env"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accepted": ["WI0116_SYNC_TOKEN"],
            "ignored": [],
            "coverage": coverage,
        })))
        .mount(server)
        .await;
}

// ─── §4a: the cold call, and the already-running branch ─────────────────────

/// **The cold-call round trip, and the already-running branch of
/// `ensure_running`.**
///
/// A client cannot know `required_env` before the daemon answers, so the
/// exchange resolves it in one pass: the first request sends *nothing* (a bare
/// GET), the answer names what the daemon wants, and the single POST that
/// follows carries the intersection of that list with this shell — never more.
#[tokio::test]
async fn ensure_running_against_a_live_daemon_learns_required_env_then_sends_the_intersection() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let salt = Salt::random();
    // The daemon holds nothing yet: two required names, no digests.
    mount_env_routes(
        &server,
        coverage_body(
            &salt,
            &[("WI0116_SYNC_TOKEN", None), ("WI0116_ABSENT", None)],
        ),
    )
    .await;

    let holder = AwmanNamedHolder::spawn();
    publish_running_daemon(tmp.path(), &server.uri(), holder.id());
    let mut env = ScopedEnv::set(&[
        ("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap()),
        (
            "AWMAN_SQUAD_ROOT",
            tmp.path().join("squad").to_str().unwrap(),
        ),
        ("WI0116_SYNC_TOKEN", "the-value"),
        // Set, but outside `required_env`: it must never be transmitted.
        ("WI0116_NOT_REQUIRED", "never-sent"),
    ]);
    env.clear("WI0116_ABSENT");
    env.clear("AWMAN_SQUAD_KEY");

    let resolver = SquadGatewayResolver::from_env(&Env::from_process()).unwrap();
    resolver
        .ensure_running()
        .await
        .expect("the already-running branch must return the sidecar's endpoint");

    let seen = requests(&server).await;
    assert_eq!(
        seen.len(),
        2,
        "one coverage check and one push, no more: {seen:?}"
    );
    assert_eq!(
        (seen[0].0.as_str(), seen[0].1.as_str()),
        ("GET", "/v1/daemon/env"),
        "the cold call sends nothing first — it asks"
    );
    assert_eq!(
        (seen[1].0.as_str(), seen[1].1.as_str()),
        ("POST", "/v1/daemon/env")
    );

    let push = &seen[1].2;
    assert_eq!(
        push["vars"],
        serde_json::json!({"WI0116_SYNC_TOKEN": "the-value"}),
        "only the intersection of `required_env` and this shell is sent: {push}"
    );
    assert_eq!(
        push["absent"],
        serde_json::json!(["WI0116_ABSENT"]),
        "a required name this shell lacks is reported absent, not omitted: {push}"
    );
    assert!(
        !push.to_string().contains("WI0116_NOT_REQUIRED"),
        "nothing outside required_env is ever transmitted: {push}"
    );

    drop(env);
    drop(holder);
}

/// Steady state: the daemon reports a digest that matches what this shell
/// holds, so `ensure_running` sends **nothing at all** beyond the check. This
/// is why a coverage sync on every command is affordable — and why looking at a
/// list does not move a secret.
#[tokio::test]
async fn a_matching_digest_costs_one_get_and_puts_no_secret_on_the_wire() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let salt = Salt::random();
    let digest = coverage_digest(&salt, "WI0116_SYNC_TOKEN", "the-value");
    mount_env_routes(
        &server,
        coverage_body(&salt, &[("WI0116_SYNC_TOKEN", Some(digest))]),
    )
    .await;

    let holder = AwmanNamedHolder::spawn();
    publish_running_daemon(tmp.path(), &server.uri(), holder.id());
    let mut env = ScopedEnv::set(&[
        ("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap()),
        (
            "AWMAN_SQUAD_ROOT",
            tmp.path().join("squad").to_str().unwrap(),
        ),
        ("WI0116_SYNC_TOKEN", "the-value"),
    ]);
    env.clear("AWMAN_SQUAD_KEY");

    let resolver = SquadGatewayResolver::from_env(&Env::from_process()).unwrap();
    resolver.ensure_running().await.expect("endpoint");

    let seen = requests(&server).await;
    assert_eq!(
        seen.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        vec!["GET"],
        "a matching digest sends nothing: {seen:?}"
    );

    drop(env);
    drop(holder);
}

/// A rotated value is the case a push exists for: the digests differ, so the
/// new value goes — over the same already-running daemon, with no restart.
#[tokio::test]
async fn a_rotated_value_is_pushed_to_an_already_running_daemon() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let salt = Salt::random();
    let stale = coverage_digest(&salt, "WI0116_SYNC_TOKEN", "the-old-value");
    mount_env_routes(
        &server,
        coverage_body(&salt, &[("WI0116_SYNC_TOKEN", Some(stale))]),
    )
    .await;

    let holder = AwmanNamedHolder::spawn();
    publish_running_daemon(tmp.path(), &server.uri(), holder.id());
    let mut env = ScopedEnv::set(&[
        ("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap()),
        (
            "AWMAN_SQUAD_ROOT",
            tmp.path().join("squad").to_str().unwrap(),
        ),
        ("WI0116_SYNC_TOKEN", "the-new-value"),
    ]);
    env.clear("AWMAN_SQUAD_KEY");

    let resolver = SquadGatewayResolver::from_env(&Env::from_process()).unwrap();
    resolver.ensure_running().await.expect("endpoint");

    let seen = requests(&server).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(
        seen[1].2["vars"],
        serde_json::json!({"WI0116_SYNC_TOKEN": "the-new-value"}),
        "a long-lived daemon picks up a rotated token from the next command"
    );

    drop(env);
    drop(holder);
}

/// `squad status` and the TUI's ten-second indicator poller use
/// `gateway_from_meta` / `probe_gateway`, which deliberately do **not** sync:
/// a read-only look at a daemon must not push anything.
#[tokio::test]
async fn a_read_only_probe_gateway_pushes_nothing() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let salt = Salt::random();
    mount_env_routes(
        &server,
        coverage_body(&salt, &[("WI0116_SYNC_TOKEN", None)]),
    )
    .await;

    let holder = AwmanNamedHolder::spawn();
    publish_running_daemon(tmp.path(), &server.uri(), holder.id());
    let mut env = ScopedEnv::set(&[
        ("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap()),
        (
            "AWMAN_SQUAD_ROOT",
            tmp.path().join("squad").to_str().unwrap(),
        ),
        ("WI0116_SYNC_TOKEN", "the-value"),
    ]);
    env.clear("AWMAN_SQUAD_KEY");

    let resolver = SquadGatewayResolver::from_env(&Env::from_process()).unwrap();
    assert!(resolver.probe_gateway().unwrap().is_some());
    assert!(resolver.gateway_from_meta().unwrap().is_some());

    assert!(
        requests(&server).await.is_empty(),
        "building a read-only gateway must not touch the daemon at all"
    );

    drop(env);
    drop(holder);
}

// ─── §6c: the indicator's probe still costs exactly one `list` ──────────────

/// The seventh indicator state costs **no second round trip**. `unmet_env` is a
/// field on `Task`, so it arrives on the very `list` the poller already makes;
/// the probe must not also fetch coverage or status.
///
/// Driven through the public `SquadIndicatorPoller`, because `probe_once` is
/// private — which is the point: the poller is the only caller, and this pins
/// what it actually does over the wire.
#[tokio::test]
async fn the_indicator_probe_makes_exactly_one_list_call_per_tick() {
    use awman::frontend::tui::squad_indicator::{SquadIndicator, SquadIndicatorPoller};

    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;

    // One task, carrying an unmet name on the `list` response itself.
    let task = Task {
        id: "deploy".into(),
        name: "deploy".into(),
        description: "d".into(),
        repo_scope: tmp.path().to_path_buf(),
        mount_scope: MountScope::GitRoot,
        overlays: vec!["env(AWS_PROFILE)".into()],
        interval_secs: 600,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        unmet_env: vec!["AWS_PROFILE".into()],
    };
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![task]))
        .mount(&server)
        .await;
    // Coverage and status are mounted too, so a probe that *did* make a second
    // call would succeed rather than error — the assertion below is then about
    // what the probe chose to do, not about what happened to be reachable.
    mount_env_routes(&server, coverage_body(&Salt::random(), &[])).await;

    let holder = AwmanNamedHolder::spawn();
    publish_running_daemon(tmp.path(), &server.uri(), holder.id());
    let mut env = ScopedEnv::set(&[
        ("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap()),
        (
            "AWMAN_SQUAD_ROOT",
            tmp.path().join("squad").to_str().unwrap(),
        ),
    ]);
    env.clear("AWMAN_SQUAD_KEY");

    let shared: Arc<std::sync::Mutex<SquadIndicator>> =
        Arc::new(std::sync::Mutex::new(SquadIndicator::Unknown));
    let cancel = tokio_util::sync::CancellationToken::new();
    let handle = SquadIndicatorPoller::new(shared.clone()).start(cancel.clone());

    // The first probe runs immediately; wait for it to land.
    let mut waited = Duration::ZERO;
    loop {
        if *shared.lock().unwrap() != SquadIndicator::Unknown {
            break;
        }
        assert!(
            waited < Duration::from_secs(5),
            "the first probe never landed"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        waited += Duration::from_millis(25);
    }
    cancel.cancel();
    let _ = handle.await;

    assert_eq!(
        *shared.lock().unwrap(),
        SquadIndicator::EnvUnmet,
        "`unmet_env` rides on the task list, so one `list` is enough to classify"
    );
    let seen = requests(&server).await;
    assert_eq!(
        seen.len(),
        1,
        "exactly one call per tick — the seventh state added no round trip: {seen:?}"
    );
    assert_eq!(
        (seen[0].0.as_str(), seen[0].1.as_str()),
        ("POST", "/v1/commands")
    );
    assert_eq!(
        seen[0].2["subcommand"], "squad list",
        "and that one call is the `list` the poller already made"
    );

    drop(env);
    drop(holder);
}

// ─── The gateway's own env surface, over the wire ───────────────────────────

/// `env_coverage()` deserialises the daemon's body into the shape every
/// frontend reads, and carries no value because the body carries none.
#[tokio::test]
async fn the_remote_gateway_reads_coverage_as_names_digests_and_timestamps() {
    let _lock = PROCESS_ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let salt = Salt::random();
    let digest = coverage_digest(&salt, "WI0116_SYNC_TOKEN", "the-value");
    mount_env_routes(
        &server,
        coverage_body(&salt, &[("WI0116_SYNC_TOKEN", Some(digest.clone()))]),
    )
    .await;

    let holder = AwmanNamedHolder::spawn();
    publish_running_daemon(tmp.path(), &server.uri(), holder.id());
    let mut env = ScopedEnv::set(&[
        ("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap()),
        (
            "AWMAN_SQUAD_ROOT",
            tmp.path().join("squad").to_str().unwrap(),
        ),
    ]);
    env.clear("AWMAN_SQUAD_KEY");

    let resolver = SquadGatewayResolver::from_env(&Env::from_process()).unwrap();
    let gateway = resolver.probe_gateway().unwrap().expect("a gateway");
    let coverage: EnvCoverage = gateway.env_coverage().await.expect("coverage");

    assert_eq!(coverage.salt, salt.to_hex());
    assert_eq!(coverage.persistence, "none");
    assert_eq!(coverage.required.len(), 1);
    assert_eq!(coverage.required[0].name, "WI0116_SYNC_TOKEN");
    assert_eq!(
        coverage.required[0].digest.as_deref(),
        Some(digest.as_str())
    );
    // The typed form has nowhere to put a value: the fields are a name, a
    // digest, three timestamps, a source and two booleans.
    let round_tripped = serde_json::to_string(&coverage).unwrap();
    assert!(!round_tripped.contains("the-value"), "{round_tripped}");

    drop(env);
    drop(holder);
}
