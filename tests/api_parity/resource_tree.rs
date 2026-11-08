use awman::command::daemon_api::{capabilities::CAPABILITIES, DaemonApi, Feature, FeatureState};
use awman::frontend::api::routes::{build_router_for_listener, ListenerPolicy, ROUTES};
use std::sync::Arc;

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("http://{addr}{path}"))
        .header("authorization", "Bearer test-key");
    if let Some(body) = body {
        request = request.json(&body)
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(serde_json::Value::Null);
    server.abort();
    (status, body)
}
#[tokio::test]
async fn removed_paths_have_no_alias_and_every_failure_has_a_code() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    let router = awman::frontend::api::routes::build_router(state);
    for path in [
        "/v1/sessions",
        "/v1/sessions/old",
        "/v1/commands",
        "/v1/commands/old/status",
        "/v1/runs",
        "/v1/workdirs",
        "/v1/workflows/old",
        "/v1/tasks/nightly/workflow",
        "/v1/daemon/env",
        "/v1/daemon/status",
        "/v1/engine/actions",
        "/v1/actions",
        "/v1/commands/old/terminals",
        // WI 0141: per-feature terminal routes are gone; `/v1/terminals` is core.
        "/v1/commands/old/terminals/term_old",
        "/v1/engine/terminals",
        "/v1/engine/terminals/term_old",
        "/v1/worker/commands/old/terminals",
        "/v1/worker/sessions/old/status",
        "/v1/worker/commands/old/status",
        "/v1/worker/commands/old/logs",
    ] {
        for method in ["GET", "POST", "PATCH", "PUT", "DELETE"] {
            let (status, body) = call(&router, method, path, None).await;
            assert_eq!(status, 404, "{method} {path}: {body}");
            assert_eq!(body["code"], "not_found");
            assert!(body["error"].is_string());
        }
    }
}
#[tokio::test]
async fn list_envelopes_capabilities_prefixed_ids_and_labels() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    let router = awman::frontend::api::routes::build_router(state);
    let (status, body) = call(&router, "GET", "/v1/status", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["features"]["worker"]["state"], "on");
    assert_eq!(body["features"]["squad"]["state"], "off");
    let capabilities: Vec<String> = serde_json::from_value(body["capabilities"].clone()).unwrap();
    assert_eq!(capabilities, CAPABILITIES);
    assert!(body["uptime_ms"].is_u64());
    assert!(body.get("uptime_seconds").is_none());
    for path in [
        "/v1/features",
        "/v1/grants",
        "/v1/worker/workdirs",
        "/v1/worker/sessions",
        "/v1/worker/commands",
    ] {
        let (status, body) = call(&router, "GET", path, None).await;
        assert_eq!(status, 200, "{path}: {body}");
        assert!(body["items"].is_array());
        assert!(body.get("next_cursor").is_some());
        if path.ends_with("sessions") {
            assert!(body["items"][0]["id"].as_str().unwrap().starts_with("ses_"));
        }
    }
    for labels in [
        serde_json::json!({"infact.kind":"x"}),
        serde_json::json!({"awman/name":"x"}),
    ] {
        for path in ["/v1/worker/sessions", "/v1/worker/commands"] {
            let(status,body)=call(&router,"POST",path,Some(serde_json::json!({"workdir":"/tmp","subcommand":"exec prompt","args":["hi"],"labels":labels}))).await;
            assert_eq!(status, 400, "{path}: {body}");
            assert_eq!(body["code"], "invalid_request");
        }
    }
    for path in [
        "/v1/worker/sessions?limit=0",
        "/v1/worker/commands?limit=501",
        "/v1/features?cursor=bad",
    ] {
        let (status, body) = call(&router, "GET", path, None).await;
        assert_eq!(status, 400);
        assert_eq!(body["code"], "invalid_request");
    }
}
#[tokio::test]
async fn listener_policy_hides_unlisted_and_owner_routes_but_reports_disabled_features() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    let api = Arc::new(DaemonApi::for_worker_tests(state));
    let (_, key) = api.runtime.keys().create("remote").unwrap();
    let router = build_router_for_listener(api, ListenerPolicy::Network);
    for auth in [None, Some("invalid"), Some(key.as_str())] {
        for (method, path, want) in [
            ("GET", "/v1/keys", 404),
            ("PATCH", "/v1/features/worker", 404),
            ("POST", "/v1/config/reload", 404),
            ("PATCH", "/v1/config", 404),
            ("GET", "/v1/squad/tasks", 404),
            ("PATCH", "/v1/squad/tasks", 404),
            (
                "GET",
                "/v1/status",
                if auth == Some(key.as_str()) { 200 } else { 401 },
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = router.clone();
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut request = reqwest::Client::new()
                .request(method.parse().unwrap(), format!("http://{addr}{path}"));
            if let Some(key) = auth {
                request = request.bearer_auth(key);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                want,
                "{method} {path}"
            );
            task.abort();
        }
    }
}
#[tokio::test]
async fn every_registered_route_resolves_to_its_owner_and_no_other_route_is_served() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    let api = DaemonApi::for_worker_tests(state);
    let file = awman::data::config::file::global_config_path(api.runtime.config().env()).unwrap();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        file,
        r#"{"daemon":{"features":{"worker":{"enabled":false}}}}"#,
    )
    .unwrap();
    api.runtime.reload_config().await.unwrap();
    tokio::task::yield_now().await;
    let router = build_router_for_listener(Arc::new(api), ListenerPolicy::Local).layer(
        axum::Extension(awman::frontend::api::local::OwnerAuthenticated),
    );
    for r in ROUTES.iter().filter(|r| r.op.owner() != Feature::Core) {
        let path = r.path.replace("{id}", "test").replace("{*path}", "file");
        let (status, body) = call(&router, r.method, &path, None).await;
        assert_eq!(status, 503, "{} {}: {body}", r.method, path);
        assert_eq!(body["code"], "feature_disabled");
    }
}

#[tokio::test]
async fn reload_validates_before_applying_and_keeps_restart_only_settings_inactive() {
    use awman::data::config::env::EnvSnapshot;
    use awman::data::config::file::global_config_path;
    use awman::data::config::{ConfigLayers, EffectiveConfig};
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(&root.path().join("api"));
    let home = root.path().join("home");
    let env = EnvSnapshot::with_overrides([("AWMAN_HOME", home.to_str().unwrap())]);
    let file = global_config_path(&env).unwrap();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        r#"{"daemon":{"features":{"worker":{"enabled":true}}}}"#,
    )
    .unwrap();
    let config = EffectiveConfig::resolve(
        ConfigLayers::load(&env, None, Default::default()).unwrap(),
        Default::default(),
        env,
    )
    .unwrap();
    let api = Arc::new(DaemonApi::for_worker_tests_with_config(
        state.clone(),
        config,
    ));
    let router = build_router_for_listener(api.clone(), ListenerPolicy::Local).layer(
        axum::Extension(awman::frontend::api::local::OwnerAuthenticated),
    );
    let allowed = root.path().join("allowed");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::write(
        &file,
        serde_json::json!({"daemon":{
            "maxConcurrentJobs":3, "verboseSetup":true, "network":{"listen":"127.0.0.1:7777"},
            "features":{"worker":{"enabled":true,"push":{"workDirs":{"allow":[allowed]}}}}
        }})
        .to_string(),
    )
    .unwrap();
    let (status, response) = call(&router, "POST", "/v1/config/reload", None).await;
    assert_eq!(status, 200, "{response}");
    assert!(response["applied"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("daemon.maxConcurrentJobs")));
    assert!(response["restart_required"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("daemon.network.listen")));
    {
        let live = state.live_config.read().unwrap();
        assert_eq!(live.as_ref().unwrap().daemon().max_concurrent_jobs, 3);
        assert!(live.as_ref().unwrap().daemon().verbose_setup);
        assert_eq!(
            live.as_ref().unwrap().daemon().network.listen.to_string(),
            "127.0.0.1:9876"
        );
    }
    let (status, response) = call(&router, "GET", "/v1/worker/workdirs", None).await;
    assert_eq!(status, 200);
    assert_eq!(response["items"][0]["path"], allowed.to_str().unwrap());
    std::fs::write(
        &file,
        serde_json::json!({"daemon":{"maxConcurrentJobs":3,"features":{"worker":{"enabled":false,"push":{"enabled":false}}}}})
            .to_string(),
    )
    .unwrap();
    let (status, response) = call(&router, "POST", "/v1/config/reload", None).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(
        api.runtime
            .feature_state(awman::data::daemon_state::FeatureName::Worker)
            .state,
        FeatureState::Off
    );
    let (status, response) = call(&router, "GET", "/v1/worker/sessions", None).await;
    assert_eq!(status, 503, "{response}");
    assert_eq!(response["code"], "feature_disabled");
    std::fs::write(
        &file,
        serde_json::json!({"daemon":{"maxConcurrentJobs":3,"features":{"worker":{"enabled":true,"push":{"enabled":true}}}}})
            .to_string(),
    )
    .unwrap();
    let (status, response) = call(&router, "POST", "/v1/config/reload", None).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(
        api.runtime
            .feature_state(awman::data::daemon_state::FeatureName::Worker)
            .state,
        FeatureState::On
    );
    let (status, response) = call(&router, "POST", "/v1/config/reload", None).await;
    assert_eq!(status, 200);
    assert_eq!(response["applied"], serde_json::json!([]));
    assert_eq!(
        state
            .live_config
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .daemon()
            .max_concurrent_jobs,
        3
    );
    std::fs::write(&file, r#"{"daemon":{"maxConcurrentJobs":0}}"#).unwrap();
    let (status, response) = call(&router, "POST", "/v1/config/reload", None).await;
    assert_eq!(status, 422, "{response}");
    assert_eq!(response["code"], "config_invalid");
    assert_eq!(
        state
            .live_config
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .daemon()
            .max_concurrent_jobs,
        3
    );
}

#[tokio::test]
async fn grants_are_paged_by_resource_id_and_delete_returns_no_alias() {
    use awman::data::fs::prerequisite_grants::{GrantKey, RememberedGrant, RememberedKind};
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("repo")).unwrap();
    let state = super::http_consistency::state(&root.path().join("api"));
    let api = Arc::new(DaemonApi::for_worker_tests(state));
    api.grant_store()
        .lock()
        .unwrap()
        .remember(
            GrantKey {
                git_root: root.path().join("repo"),
                remote_url: "https://example.com/repo".into(),
                prerequisite: RememberedKind::Ssh,
            },
            RememberedGrant::SshDir,
        )
        .unwrap();
    let home = api
        .engines
        .auth_engine
        .api_paths()
        .data_paths()
        .home()
        .to_path_buf();
    let router = build_router_for_listener(api, ListenerPolicy::Local).layer(axum::Extension(
        awman::frontend::api::local::OwnerAuthenticated,
    ));
    let (status, grants) = call(&router, "GET", "/v1/grants?limit=1", None).await;
    assert_eq!(status, 200, "{grants}");
    assert!(grants["next_cursor"].is_null());
    let id = grants["items"][0]["id"].as_str().unwrap();
    assert!(id.starts_with("grant_"));
    assert!(grants["items"][0]["recorded_at"]
        .as_str()
        .unwrap()
        .ends_with('Z'));
    // A workflow records approvals through its own store after daemon startup.
    let mut external = awman::data::fs::prerequisite_grants::PrerequisiteGrantStore::load(&home);
    external
        .remember(
            GrantKey {
                git_root: root.path().join("repo"),
                remote_url: "https://example.com/repo".into(),
                prerequisite: RememberedKind::GithubToken,
            },
            RememberedGrant::TokenEnv {
                var: "GH_TOKEN".into(),
            },
        )
        .unwrap();
    let (status, updated) = call(&router, "GET", "/v1/grants?limit=1", None).await;
    assert_eq!(status, 200, "{updated}");
    assert!(updated["next_cursor"].is_string());
    assert_eq!(
        call(&router, "DELETE", &format!("/v1/grants/{id}"), None)
            .await
            .0,
        204
    );
    let (status, body) = call(&router, "DELETE", &format!("/v1/grants/{id}"), None).await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "grant_not_found");
    assert_eq!(call(&router, "DELETE", "/v1/grants/1", None).await.0, 404);
}
