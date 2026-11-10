//! Route tests for the seven pull routes and the `/v1/status` worker shape
//! (WI 0143), over a fake Layer 2 pull worker.
use super::*;
use crate::command::daemon_api::worker_pull::{PullHandle, QueueRow, ServerStatus};
use crate::data::config::work_servers::ClaimMode;
use crate::data::wire::List;
use crate::data::worker::lease_store::LeaseStore;
use crate::data::worker::{LeaseHost, LeaseRecord, LeaseRelease, LeaseState};
use std::sync::Mutex;

struct FakePull {
    leases: LeaseStore,
    extra_servers: std::sync::atomic::AtomicUsize,
    claims: Mutex<Vec<(String, Option<String>)>>,
    releases: Mutex<Vec<(String, awman_wsp::ReleaseReason, Option<String>)>>,
}

fn row(id: &str, priority: i32) -> QueueRow {
    QueueRow::from_wsp(
        "team",
        awman_wsp::QueueItem {
            body: None,
            source: None,
            id: id.into(),
            title: format!("item {id}"),
            summary: Some("do the thing".into()),
            kind: awman_wsp::WorkKind::Workflow,
            labels: [("team".to_string(), "core".to_string())].into(),
            priority,
            enqueued_at: chrono::Utc::now() - chrono::Duration::seconds(5),
            assigned_to_me: priority > 0,
        },
        chrono::Utc::now(),
    )
}

#[async_trait::async_trait]
impl PullHandle for FakePull {
    fn servers(&self) -> Vec<ServerStatus> {
        let status = ServerStatus {
            name: "team".into(),
            state: "registered".into(),
            enabled: true,
            claim: ClaimMode::Manual,
            max_concurrent_jobs: 1,
            running: self.leases.active_for_server("team").unwrap().len(),
            queued_eligible: 2,
            labels: [("awman/os".to_string(), "linux".to_string())].into(),
            server_labels: [("owner".to_string(), "sam".to_string())].into(),
            last_claim_at: None,
            error: None,
        };
        let mut servers = vec![status.clone()];
        for index in 0..self.extra_servers.load(std::sync::atomic::Ordering::SeqCst) {
            let mut extra = status.clone();
            extra.name = format!("server-{index}");
            servers.push(extra);
        }
        servers
    }
    async fn queue(
        &self,
        _server: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<List<QueueRow>, WorkerError> {
        let all = [row("a", 1), row("b", 0)];
        let start = usize::from(cursor == Some("next"));
        let items: Vec<_> = all.into_iter().skip(start).take(limit).collect();
        let next_cursor = (start + items.len() < 2).then(|| "next".to_string());
        Ok(List { items, next_cursor })
    }
    async fn claim(&self, item_id: &str, server: Option<&str>) -> Result<LeaseRecord, WorkerError> {
        self.claims
            .lock()
            .unwrap()
            .push((item_id.into(), server.map(str::to_string)));
        match item_id {
            "taken" => return Err(WorkerError::Taken),
            "stale" => return Err(WorkerError::Ineligible("labels changed".into())),
            "busy" => return Err(WorkerError::Busy("no free slot".into())),
            "gone" => return Err(WorkerError::ItemNotFound("gone".into())),
            _ => {}
        }
        let wire = awman_wsp::Lease {
            id: format!("srv-{item_id}"),
            epoch: 3,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            item: awman_wsp::conformance::work_item(item_id),
            terminal: true,
            remote_input: false,
        };
        let lease = LeaseRecord::claimed("team", &wire, LeaseHost::Daemon, chrono::Utc::now());
        self.leases.insert(&lease)?;
        Ok(lease)
    }
    async fn release(
        &self,
        lease: &LeaseRecord,
        reason: awman_wsp::ReleaseReason,
        message: Option<String>,
    ) -> Result<LeaseRecord, WorkerError> {
        self.releases
            .lock()
            .unwrap()
            .push((lease.id.to_string(), reason, message.clone()));
        let mut lease = lease.clone();
        lease.state = LeaseState::Released;
        lease.ended_at = Some(chrono::Utc::now());
        lease.release = Some(LeaseRelease { reason, message });
        self.leases.update(&lease)?;
        Ok(lease)
    }
    fn capabilities(&self) -> Vec<&'static str> {
        vec!["lease_read_only", "worker_pull"]
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    api: Arc<DaemonApi>,
    fake: Option<Arc<FakePull>>,
}

fn fixture(with_pull: bool, pull_enabled: bool) -> Fixture {
    use crate::data::config::{config_json::set_config_field, ConfigFile, ConfigLayers, FileKind};
    let tmp = tempfile::tempdir().unwrap();
    let state = super::super::routes::tests::make_test_state(tmp.path());
    let mut doc = state.engines.global_config.to_value();
    for (field, value) in [
        ("daemon.features.worker.enabled", serde_json::json!(true)),
        (
            "daemon.features.worker.pull.enabled",
            serde_json::json!(pull_enabled),
        ),
        (
            "workServers.team.url",
            serde_json::json!("https://wsp.example"),
        ),
        ("workServers.team.claim", serde_json::json!("manual")),
    ] {
        set_config_field(&mut doc, field, value);
    }
    let config = crate::data::config::EffectiveConfig::resolve(
        ConfigLayers {
            global: ConfigFile::from_value(doc, FileKind::Global).unwrap(),
            ..Default::default()
        },
        Default::default(),
        crate::data::config::env::EnvSnapshot::with_overrides([(
            "AWMAN_HOME",
            tmp.path().to_string_lossy().as_ref(),
        )]),
    )
    .unwrap();
    let fake = with_pull.then(|| {
        Arc::new(FakePull {
            leases: LeaseStore::open(state.store.clone()).unwrap(),
            extra_servers: Default::default(),
            claims: Mutex::new(Vec::new()),
            releases: Mutex::new(Vec::new()),
        })
    });
    let mut api = DaemonApi::for_worker_tests_with_config(state, config);
    if let Some(fake) = &fake {
        api = api.with_pull(fake.clone());
    }
    Fixture {
        _tmp: tmp,
        api: Arc::new(api),
        fake,
    }
}

async fn serve(
    api: Arc<DaemonApi>,
    policy: super::super::routes::ListenerPolicy,
) -> Option<std::net::SocketAddr> {
    let local = matches!(policy, super::super::routes::ListenerPolicy::Local);
    let mut app = super::super::routes::build_router_for_listener(api, policy);
    if local {
        app = app.layer(axum::Extension(super::super::local::OwnerAuthenticated));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Some(addr)
}

async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let url = format!("http://{addr}{path}");
    let mut req = match method {
        "GET" => client.get(url),
        "POST" => client.post(url),
        _ => unreachable!(),
    };
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

fn assert_error(status: u16, body: &serde_json::Value, want_status: u16, code: &str) {
    assert_eq!(status, want_status, "{body}");
    assert_eq!(body["code"], code, "{body}");
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn servers_routes_report_connection_state_per_work_server() {
    let f = fixture(true, true);
    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await else {
        return;
    };
    let (status, body) = call(addr, "GET", "/v1/worker/servers", None).await;
    assert_eq!(status, 200, "{body}");
    assert!(body["next_cursor"].is_null(), "{body}");
    let team = &body["items"][0];
    assert_eq!(team["name"], "team");
    assert_eq!(team["state"], "registered");
    assert_eq!(team["claim"], "manual");
    assert_eq!(team["server_labels"]["owner"], "sam");
    assert_eq!(team["labels"]["awman/os"], "linux");
    assert!(team["last_claim_at"].is_null());

    let (status, body) = call(addr, "GET", "/v1/worker/servers/team", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["queued_eligible"], 2);
    let (status, body) = call(addr, "GET", "/v1/worker/servers/nope", None).await;
    assert_error(status, &body, 404, "server_not_found");
}

#[tokio::test]
async fn servers_without_a_pull_worker_come_from_config() {
    let f = fixture(false, true);
    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await else {
        return;
    };
    let (status, body) = call(addr, "GET", "/v1/worker/servers/team", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "disconnected");
    assert_eq!(body["running"], 0);
    assert!(body["error"].is_string());
    let off = fixture(false, false);
    let Some(addr) = serve(off.api.clone(), super::super::routes::ListenerPolicy::Local).await
    else {
        return;
    };
    let (_, body) = call(addr, "GET", "/v1/worker/servers/team", None).await;
    assert_eq!(body["state"], "disabled");
}

#[tokio::test]
async fn queue_lists_eligible_items_as_items_and_next_cursor() {
    let f = fixture(true, true);
    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await else {
        return;
    };
    let (status, body) = call(addr, "GET", "/v1/worker/queue?server=team&limit=1", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], "a");
    assert_eq!(body["items"][0]["server"], "team");
    assert_eq!(body["items"][0]["kind"], "workflow");
    assert_eq!(body["items"][0]["for_you"], true);
    assert!(body["items"][0]["age_ms"].as_u64().unwrap() >= 4000);
    assert!(body["items"][0]["enqueued_at"].is_string());
    assert!(body["items"][0].get("body").is_none());
    assert_eq!(body["next_cursor"], "next");
    let (_, body) = call(addr, "GET", "/v1/worker/queue?cursor=next", None).await;
    assert_eq!(body["items"][0]["id"], "b");
    assert!(body["next_cursor"].is_null());

    let (status, body) = call(addr, "GET", "/v1/worker/queue?server=nope", None).await;
    assert_error(status, &body, 404, "server_not_found");
    let (status, body) = call(addr, "GET", "/v1/worker/queue?limit=0", None).await;
    assert_error(status, &body, 400, "invalid_request");
    let (status, body) = call(addr, "GET", "/v1/worker/queue?bogus=1", None).await;
    assert_error(status, &body, 400, "invalid_request");
}

#[tokio::test]
async fn queue_and_claim_are_refused_while_pull_is_off_or_not_running() {
    for (with_pull, enabled) in [(true, false), (false, true)] {
        let f = fixture(with_pull, enabled);
        let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await
        else {
            return;
        };
        let (status, body) = call(addr, "GET", "/v1/worker/queue", None).await;
        assert_error(status, &body, 503, "pull_disabled");
        let (status, body) = call(addr, "POST", "/v1/worker/queue/a/claim", None).await;
        assert_error(status, &body, 503, "pull_disabled");
        // Leases are durable and stay readable.
        let (status, body) = call(addr, "GET", "/v1/worker/leases", None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["items"], serde_json::json!([]));
    }
}

#[tokio::test]
async fn claim_returns_a_lease_resource_with_lease_ids_and_server_lease_id() {
    let f = fixture(true, true);
    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await else {
        return;
    };
    let (status, body) = call(addr, "POST", "/v1/worker/queue/01J/claim?server=team", None).await;
    assert_eq!(status, 201, "{body}");
    let id = body["id"].as_str().unwrap();
    assert!(crate::data::ids::LeaseId::parse(id).is_ok(), "{id}");
    assert_eq!(body["server_lease_id"], "srv-01J");
    assert_eq!(body["server"], "team");
    assert_eq!(body["epoch"], 3);
    assert_eq!(body["item_id"], "01J");
    assert_eq!(body["state"], "claimed");
    assert_eq!(body["host"]["kind"], "daemon");
    assert!(body["claimed_at"].is_string() && body["expires_at"].is_string());
    assert_eq!(
        f.fake.as_ref().unwrap().claims.lock().unwrap()[0],
        ("01J".to_string(), Some("team".to_string()))
    );

    for (item, want, code) in [
        ("taken", 409, "taken"),
        ("stale", 409, "ineligible"),
        ("busy", 409, "worker_busy"),
        ("gone", 404, "item_not_found"),
    ] {
        let (status, body) = call(
            addr,
            "POST",
            &format!("/v1/worker/queue/{item}/claim"),
            None,
        )
        .await;
        assert_error(status, &body, want, code);
    }
}

#[tokio::test]
async fn leases_list_get_and_release_follow_the_lease_lifecycle() {
    let f = fixture(true, true);
    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await else {
        return;
    };
    let mut ids = Vec::new();
    for item in ["one", "two", "three"] {
        let (_, body) = call(
            addr,
            "POST",
            &format!("/v1/worker/queue/{item}/claim"),
            None,
        )
        .await;
        ids.push(body["id"].as_str().unwrap().to_string());
    }
    let (status, page) = call(addr, "GET", "/v1/worker/leases?limit=2", None).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let cursor = page["next_cursor"].as_str().unwrap().to_string();
    let (_, rest) = call(
        addr,
        "GET",
        &format!("/v1/worker/leases?cursor={cursor}"),
        None,
    )
    .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert!(rest["next_cursor"].is_null());
    let (_, by_server) = call(addr, "GET", "/v1/worker/leases?server=other", None).await;
    assert_eq!(by_server["items"], serde_json::json!([]));

    let (status, one) = call(addr, "GET", &format!("/v1/worker/leases/{}", ids[0]), None).await;
    assert_eq!(status, 200, "{one}");
    assert_eq!(one["server_lease_id"], "srv-one");

    let release = format!("/v1/worker/leases/{}/release", ids[0]);
    let (status, released) = call(addr, "POST", &release, None).await;
    assert_eq!(status, 200, "{released}");
    assert_eq!(released["state"], "released");
    assert_eq!(released["release_reason"], "user_declined");
    let (status, body) = call(addr, "POST", &release, None).await;
    assert_error(status, &body, 409, "lease_ended");

    let (status, released) = call(
        addr,
        "POST",
        &format!("/v1/worker/leases/{}/release", ids[1]),
        Some(serde_json::json!({"reason": "error", "message": "stopped"})),
    )
    .await;
    assert_eq!(status, 200, "{released}");
    assert_eq!(released["release_reason"], "error");
    assert_eq!(
        f.fake.as_ref().unwrap().releases.lock().unwrap()[1]
            .2
            .as_deref(),
        Some("stopped")
    );
    let (status, body) = call(
        addr,
        "POST",
        &format!("/v1/worker/leases/{}/release", ids[2]),
        Some(serde_json::json!({"reason": "nope"})),
    )
    .await;
    assert_error(status, &body, 400, "invalid_request");

    let (_, active) = call(addr, "GET", "/v1/worker/leases?state=active", None).await;
    assert_eq!(active["items"].as_array().unwrap().len(), 1);
    let (_, done) = call(addr, "GET", "/v1/worker/leases?state=released", None).await;
    assert_eq!(done["items"].as_array().unwrap().len(), 2);
    let (status, body) = call(addr, "GET", "/v1/worker/leases?state=bogus", None).await;
    assert_error(status, &body, 400, "invalid_request");

    let missing = crate::data::ids::LeaseId::new();
    let (status, body) = call(addr, "GET", &format!("/v1/worker/leases/{missing}"), None).await;
    assert_error(status, &body, 404, "lease_not_found");
    let (status, body) = call(
        addr,
        "POST",
        &format!("/v1/worker/leases/{missing}/release"),
        None,
    )
    .await;
    assert_error(status, &body, 404, "lease_not_found");
    // A malformed lease ID is not a resource path at all.
    let (status, body) = call(addr, "GET", "/v1/worker/leases/cmd_x", None).await;
    assert_error(status, &body, 404, "not_found");
}

#[tokio::test]
async fn pull_routes_are_local_only() {
    let f = fixture(true, true);
    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Network).await
    else {
        return;
    };
    for (method, path) in [
        ("GET", "/v1/worker/servers"),
        ("GET", "/v1/worker/servers/team"),
        ("GET", "/v1/worker/queue"),
        ("POST", "/v1/worker/queue/a/claim"),
        ("GET", "/v1/worker/leases"),
        (
            "GET",
            "/v1/worker/leases/lease_00000000000000000000000000000000",
        ),
        (
            "POST",
            "/v1/worker/leases/lease_00000000000000000000000000000000/release",
        ),
    ] {
        let (status, body) = call(addr, method, path, None).await;
        assert_error(status, &body, 404, "not_found");
    }
    assert!(f.fake.unwrap().claims.lock().unwrap().is_empty());
}

#[tokio::test]
async fn status_reports_worker_push_and_pull_and_pull_capabilities_only_when_running() {
    let f = fixture(true, true);
    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await else {
        return;
    };
    call(addr, "POST", "/v1/worker/queue/x/claim", None).await;
    let (status, body) = call(addr, "GET", "/v1/status", None).await;
    assert_eq!(status, 200, "{body}");
    let worker = &body["features"]["worker"];
    for field in [
        "state",
        "max_concurrent_jobs",
        "running",
        "queued",
        "oldest_queued_age_ms",
    ] {
        assert!(worker.get(field).is_some(), "{field}: {worker}");
    }
    assert_eq!(
        worker["push"],
        serde_json::json!({"running": 0, "queued": 0})
    );
    assert_eq!(
        worker["pull"],
        serde_json::json!({"servers": [
            {"name": "team", "state": "registered", "running": 1, "queued_eligible": 2}
        ]})
    );
    assert!(body["features"]["squad"].get("push").is_none());
    let caps: Vec<&str> = body["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for cap in [
        "workflow_dynamic",
        "workflow_file",
        "prompt",
        "terminal",
        "interactive",
        "outputs_mount_state",
        "workflow_state_events",
        "worker_pull",
        "lease_read_only",
    ] {
        assert!(caps.contains(&cap), "{cap}: {caps:?}");
    }
    assert!(!caps.contains(&"lease_push_retry"), "{caps:?}");
    let at = |c: &str| caps.iter().position(|x| *x == c).unwrap();
    assert!(at("worker_pull") < at("lease_read_only"));

    let none = fixture(false, true);
    let Some(addr) = serve(
        none.api.clone(),
        super::super::routes::ListenerPolicy::Local,
    )
    .await
    else {
        return;
    };
    let (_, body) = call(addr, "GET", "/v1/status", None).await;
    let caps = body["capabilities"].as_array().unwrap();
    for cap in crate::command::daemon_api::capabilities::PULL_CAPABILITIES {
        assert!(!caps.contains(&serde_json::json!(cap)), "{cap}");
    }
    assert_eq!(
        body["features"]["worker"]["pull"]["servers"][0]["state"],
        "disconnected"
    );
}

#[tokio::test]
async fn push_status_counts_pushed_commands_and_not_pulled_ones() {
    let f = fixture(false, true);
    let worker = f.api.worker.clone().unwrap();
    worker
        .store
        .insert_session_full(crate::data::fs::api_db::NewSessionRow {
            id: "ses_03c7c0ace395d80182db07ae2c30f034",
            workdir: "/work",
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: crate::data::session_setup_event::SessionSetupStatus::Ready,
            kind: crate::data::session::SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    for id in [
        "cmd_4a8a08f09d37b73795649038408b5f33",
        "cmd_5a8a08f09d37b73795649038408b5f33",
    ] {
        worker
            .store
            .enqueue_command(
                id,
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "exec prompt",
                "[]",
                "/unused",
            )
            .unwrap();
    }
    let jobs = crate::command::daemon_api::worker::job_store(&worker).unwrap();
    jobs.insert(
        "cmd_5a8a08f09d37b73795649038408b5f33",
        &crate::data::worker::JobOrigin::Pull {
            server: "team".into(),
            lease_id: "lease_1".into(),
            epoch: 1,
        },
        &crate::data::worker::JobSpec {
            command: vec!["exec".into(), "prompt".into()],
            args: vec![],
            interaction: Default::default(),
            workspace: crate::data::worker::JobWorkspace::Local {
                workdir: "/work".into(),
            },
        },
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    jobs.set_outputs_mounted("cmd_4a8a08f09d37b73795649038408b5f33", true)
        .unwrap();
    let push = crate::command::daemon_api::worker::push_status(&worker).unwrap();
    assert_eq!(push.queued, 1);
    assert_eq!(push.running, 0);

    let Some(addr) = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local).await else {
        return;
    };
    let (status, pushed) = call(
        addr,
        "GET",
        "/v1/worker/commands/cmd_4a8a08f09d37b73795649038408b5f33",
        None,
    )
    .await;
    assert_eq!(status, 200, "{pushed}");
    assert_eq!(pushed["outputs_mounted"], true);
    assert!(pushed.get("origin").is_none(), "{pushed}");
    let (_, pulled) = call(
        addr,
        "GET",
        "/v1/worker/commands/cmd_5a8a08f09d37b73795649038408b5f33",
        None,
    )
    .await;
    assert_eq!(pulled["origin"]["kind"], "pull");
    assert_eq!(pulled["origin"]["server"], "team");
    assert!(pulled.get("outputs_mounted").is_none(), "{pulled}");
}

#[tokio::test]
async fn server_list_cursor_can_be_followed_through_the_actual_route() {
    let f = fixture(true, true);
    f.fake
        .as_ref()
        .unwrap()
        .extra_servers
        .store(50, std::sync::atomic::Ordering::SeqCst);
    let addr = serve(f.api.clone(), super::super::routes::ListenerPolicy::Local)
        .await
        .unwrap();
    let (status, first) = call(addr, "GET", "/v1/worker/servers", None).await;
    assert_eq!(status, 200);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, last) = call(
        addr,
        "GET",
        &format!("/v1/worker/servers?limit=1&cursor={cursor}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{last}");
    assert_eq!(last["items"].as_array().unwrap().len(), 1);
    assert!(last["next_cursor"].is_null());
}
