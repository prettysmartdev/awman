//! The optional network listener (`daemon.network.*`).
//!
use serde_json::json;

use crate::api_process::ApiHome;
use crate::support::*;

fn taken_port() -> (std::net::TcpListener, u16) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// A listen failure fails only the network listener: the local listener and
/// every feature keep serving, and status names the address and the error.
#[tokio::test(flavor = "multi_thread")]
async fn a_network_listener_that_cannot_bind_leaves_the_local_listener_and_features_up() {
    let (_taken, port) = taken_port();
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"network":{"listeners":{"default":{
        "enabled": true,
        "listen": format!("127.0.0.1:{port}"),
    }}}}}));
    let server = home.start().await;

    let status = wait_for_status(&server, "network bind failure", |s| {
        s["listeners"]["network"]["default"]["error"].is_string()
    })
    .await;
    assert_eq!(status["status"], "ok", "{status}");
    assert_eq!(status["features"]["worker"]["state"], "on", "{status}");
    assert_eq!(status["listeners"]["local"]["transport"], "unix_socket");
    let network = &status["listeners"]["network"]["default"];
    assert!(network["error"].is_string(), "{status}");
    assert_eq!(network["listen"], format!("127.0.0.1:{port}"), "{status}");
    // The persisted value is untouched and the CLI still reaches the daemon.
    let text = home.cli_ok(&["daemon", "status"]);
    assert!(text.contains("default"), "{text}");
    home.cli_ok(&["daemon", "api", "features.list"]);
}

/// With the network listener on, health is open and everything else needs a
/// key; `/v1/keys` is local-only and a key cannot manage keys.
#[tokio::test(flavor = "multi_thread")]
async fn network_listener_serves_health_openly_and_the_rest_only_with_a_key() {
    let (taken, port) = taken_port();
    drop(taken);
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"network":{"listeners":{"default":{
        "enabled": true,
        "listen": format!("127.0.0.1:{port}"),
        "features": ["worker"],
    }}}}}));
    let server = home.start().await;
    wait_for_status(&server, "network bound", |s| {
        s["listeners"]["network"]["default"]["bound"].is_string()
    })
    .await;
    let created: serde_json::Value = serde_json::from_str(&home.cli_ok(&[
        "daemon",
        "api",
        "keys.create",
        "name=remote",
        "--json",
    ]))
    .unwrap();
    let key = created["key"].as_str().unwrap().to_string();

    // The certificate is self-signed; pin it from the daemon's own TLS dir.
    let cert = std::fs::read(
        home.home()
            .join(".local/share/awman/data/daemon/tls/cert.pem"),
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let base = format!("https://localhost:{port}");
    assert_eq!(
        client
            .get(format!("{base}/v1/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/status"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let authed = client
        .get(format!("{base}/v1/status"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), 200);
    // Local-only routes do not exist on the network listener.
    let keys = client
        .get(format!("{base}/v1/keys"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(keys.status(), 404);
    // A feature outside `network.features` is not served there.
    let squad = client
        .get(format!("{base}/v1/squad/tasks"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(squad.status(), 404);
    let _ = server;
}

/// `daemon.network.listeners.default.enabled` toggles live; the local listener stays up. A new
/// `listen` value applies on restart only.
#[tokio::test(flavor = "multi_thread")]
async fn network_enabled_toggles_live_and_listen_applies_on_restart() {
    let (taken, port) = taken_port();
    drop(taken);
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"network":{"listeners":{"default":{"listen": format!("127.0.0.1:{port}")}}}}}));
    let server = home.start().await;
    assert!(server.get("/v1/status").await["listeners"]["network"]["default"].is_null());

    let out = home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.default.enabled",
        "true",
        "--global",
    ]);
    assert!(out.contains("applied to the running daemon"), "{out}");
    let status = wait_for_status(&server, "network bound", |s| {
        s["listeners"]["network"]["default"]["bound"].is_string()
    })
    .await;
    assert!(status["listeners"]["network"]["default"]["tls"]["fingerprint_sha256"].is_string());
    assert_eq!(status["listeners"]["local"]["transport"], "unix_socket");

    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.default.enabled",
        "false",
        "--global",
    ]);
    wait_for_status(&server, "network closed", |s| {
        s["listeners"]["network"]["default"].is_null()
    })
    .await;
    assert_eq!(server.get("/v1/status").await["status"], "ok");
    let (taken, next_port) = taken_port();
    drop(taken);
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.default.enabled",
        "true",
        "--global",
    ]);
    wait_for_status(&server, "original address rebound", |s| {
        s["listeners"]["network"]["default"]["bound"].is_string()
    })
    .await;
    let next = format!("127.0.0.1:{next_port}");
    let output = home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.default.listen",
        &next,
        "--global",
    ]);
    assert!(output.contains("restart"), "{output}");
    assert_eq!(
        server.get("/v1/status").await["listeners"]["network"]["default"]["listen"],
        format!("127.0.0.1:{port}")
    );
    drop(server);
    let restarted = home.start().await;
    let status = wait_for_status(&restarted, "new address bound", |s| {
        s["listeners"]["network"]["default"]["bound"] == next
    })
    .await;
    assert_eq!(status["listeners"]["network"]["default"]["listen"], next);
    assert_eq!(
        tls_client(&home)
            .get(format!("https://localhost:{next_port}/v1/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err());
}

fn tls_client(home: &ApiHome) -> reqwest::Client {
    let cert = std::fs::read(
        home.home()
            .join(".local/share/awman/data/daemon/tls/cert.pem"),
    )
    .unwrap();
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(3))
        .add_root_certificate(reqwest::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn live_exposure_excludes_ephemeral_features_and_revocation_affects_the_next_request() {
    let (taken, port) = taken_port();
    drop(taken);
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{
        "features":{"squad":{"envPersistence":"none"}},
        "network":{"listeners":{"default":{"enabled":true,"listen":format!("127.0.0.1:{port}"),"features":["worker","squad"]}}}
    }}));
    let server = home.start().await;
    wait_for_status(&server, "network bound", |s| {
        s["listeners"]["network"]["default"]["bound"].is_string()
    })
    .await;
    let (status, created) = http(
        &server,
        reqwest::Method::POST,
        "/v1/keys",
        None,
        Some(json!({"name":"remote"})),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let key = created["key"].as_str().unwrap();
    let client = tls_client(&home);
    let base = format!("https://localhost:{port}");
    assert_eq!(
        client
            .get(format!("{base}/v1/worker/workdirs"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let (status, ephemeral) = http(
        &server,
        reqwest::Method::PATCH,
        "/v1/features/squad",
        None,
        Some(json!({"enabled":true,"ephemeral":true})),
    )
    .await;
    assert_eq!(status, 200, "{ephemeral}");
    assert_eq!(
        server.get("/v1/status").await["listeners"]["network"]["default"]["features"],
        json!(["worker"])
    );
    for features in ["", "squad"] {
        home.cli_ok(&[
            "config",
            "set",
            "daemon.network.listeners.default.features",
            features,
            "--global",
        ]);
        for auth in [None, Some("invalid"), Some(key)] {
            for (method, path) in [
                ("GET", "/v1/worker/workdirs"),
                ("POST", "/v1/worker/sessions"),
                ("PATCH", "/v1/worker/workdirs"),
                ("GET", "/v1/squad/tasks"),
                ("POST", "/v1/squad/tasks"),
                ("OPTIONS", "/v1/squad/tasks"),
            ] {
                let mut request = client.request(method.parse().unwrap(), format!("{base}{path}"));
                if let Some(key) = auth {
                    request = request.bearer_auth(key);
                }
                assert_eq!(
                    request.send().await.unwrap().status(),
                    404,
                    "{method} {path}"
                );
            }
            for (method, path) in [
                ("GET", "/v1/keys"),
                ("PATCH", "/v1/features/worker"),
                ("POST", "/v1/config/reload"),
                ("OPTIONS", "/v1/config"),
            ] {
                let mut request = client.request(method.parse().unwrap(), format!("{base}{path}"));
                if let Some(key) = auth {
                    request = request.bearer_auth(key);
                }
                assert_eq!(
                    request.send().await.unwrap().status(),
                    404,
                    "{method} {path}"
                );
            }
        }
    }
    // Reuse the authenticated client's connection across deletion of its last key.
    assert_eq!(
        client
            .get(format!("{base}/v1/status"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let (status, _) = http(
        &server,
        reqwest::Method::DELETE,
        &format!("/v1/keys/{}", created["id"].as_str().unwrap()),
        None,
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(
        client
            .get(format!("{base}/v1/status"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        server.get("/v1/status").await["listeners"]["network"]["default"]["api_keys"],
        0
    );
    assert!(home.cli_ok(&["daemon", "status"]).contains("no API keys"));
    assert_eq!(
        client
            .get(format!("{base}/v1/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_sans_reloads_the_running_tls_listener_with_verified_names() {
    let (taken, port) = taken_port();
    drop(taken);
    let home = ApiHome::new();
    home.write_config(
        json!({"daemon":{"network":{"listeners":{"default":{"enabled":true,"listen":format!("127.0.0.1:{port}")}}}}}),
    );
    let server = home.start().await;
    let before = wait_for_status(&server, "network bound", |s| {
        s["listeners"]["network"]["default"]["bound"].is_string()
    })
    .await;
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.default.tlsSans",
        "new.example.test",
        "--global",
    ]);
    let after = server.get("/v1/status").await;
    assert_ne!(
        before["listeners"]["network"]["default"]["tls"]["fingerprint_sha256"],
        after["listeners"]["network"]["default"]["tls"]["fingerprint_sha256"]
    );
    let cert = std::fs::read(
        home.home()
            .join(".local/share/awman/data/daemon/tls/cert.pem"),
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(2))
        .pool_max_idle_per_host(0)
        .add_root_certificate(reqwest::Certificate::from_pem(&cert).unwrap())
        .resolve(
            "new.example.test",
            format!("127.0.0.1:{port}").parse().unwrap(),
        )
        .resolve(
            "wrong.example.test",
            format!("127.0.0.1:{port}").parse().unwrap(),
        )
        .build()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(response) = client
            .get(format!("https://new.example.test:{port}/v1/health"))
            .send()
            .await
        {
            assert_eq!(response.status(), 200);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "listener did not load replacement certificate"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(client
        .get(format!("https://wrong.example.test:{port}/v1/health"))
        .send()
        .await
        .is_err());
    assert!(reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("https://localhost:{port}/v1/health"))
        .send()
        .await
        .is_err());
}
