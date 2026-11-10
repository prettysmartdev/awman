//! Named listener isolation, live policy and independent failures.
use crate::{api_process::ApiHome, support::*};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
fn key(home: &ApiHome) -> String {
    let value: Value = serde_json::from_str(&home.cli_ok(&[
        "daemon",
        "api",
        "keys.create",
        "name=named",
        "--json",
    ]))
    .unwrap();
    value["key"].as_str().unwrap().into()
}
fn tls_client(home: &ApiHome) -> reqwest::Client {
    let cert = std::fs::read(
        home.home()
            .join(".local/share/awman/data/daemon/tls/cert.pem"),
    )
    .unwrap();
    reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap()
}
#[tokio::test(flavor = "multi_thread")]
async fn live_loopback_changes_leave_public_tls_worker_serving_and_report_transport_disconnects() {
    let worker = port();
    let local = port();
    let home = ApiHome::new();
    home.write_config(
        json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{
            "default":{"enabled":true,"listen":format!("0.0.0.0:{worker}")},
            "loopback":{"listen":format!("127.0.0.1:{local}")}
        }}}}),
    );
    let server = home.start().await;
    wait_for_status(&server, "TLS worker bound", |s| {
        s["listeners"]["network"]["default"]["bound"].is_string()
    })
    .await;
    let key = key(&home);
    let tls = tls_client(&home);
    // A connection that has not sent ClientHello remains open across changes
    // to the other listener, detecting accidental global shutdown/rebind.
    let mut worker_socket = tokio::net::TcpStream::connect(("127.0.0.1", worker))
        .await
        .unwrap();
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.enabled",
        "true",
        "--global",
    ]);
    wait_for_status(&server, "loopback bound", |s| {
        s["listeners"]["network"]["loopback"]["bound"].is_string()
    })
    .await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    for (base, yes, no) in [
        (
            format!("https://localhost:{worker}"),
            "/v1/worker/workdirs",
            "/v1/engine/instances",
        ),
        (
            format!("http://127.0.0.1:{local}"),
            "/v1/engine/instances",
            "/v1/worker/workdirs",
        ),
    ] {
        let client = if base.starts_with("https") {
            &tls
        } else {
            &http
        };
        assert_eq!(
            client
                .get(format!("{base}{yes}"))
                .bearer_auth(&key)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .get(format!("{base}{no}"))
                .bearer_auth(&key)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let mut old_http = tokio::net::TcpStream::connect(("127.0.0.1", local))
        .await
        .unwrap();
    old_http
        .write_all(format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{local}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let mut response = Vec::new();
    while !response.ends_with(b"{\"status\":\"ok\"}") {
        let count = old_http.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        response.extend_from_slice(&buffer[..count]);
    }
    let instance: Value = http
        .post(format!("http://127.0.0.1:{local}/v1/engine/instances"))
        .bearer_auth(&key)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut upgraded = tokio::net::TcpStream::connect(("127.0.0.1", local))
        .await
        .unwrap();
    let upgrade = format!("GET /v1/engine/instances/{}/frontend HTTP/1.1\r\nHost: 127.0.0.1:{local}\r\nAuthorization: Bearer {key}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: awman.frontend.v1\r\n\r\n", instance["id"].as_str().unwrap());
    upgraded.write_all(upgrade.as_bytes()).await.unwrap();
    let mut switching = Vec::new();
    while !switching.ends_with(b"\r\n\r\n") {
        let count = upgraded.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        switching.extend_from_slice(&buffer[..count]);
    }
    assert!(
        switching.starts_with(b"HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&switching)
    );
    let output = home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.transport",
        "tls",
        "--global",
    ]);
    assert!(output.contains("listener loopback rebound"), "{output}");
    assert!(output.contains("clients disconnected"), "{output}");
    assert!(!output.contains("0 clients disconnected"), "{output}");
    let closed = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        upgraded.read(&mut buffer),
    )
    .await
    .unwrap();
    assert!(
        closed.map(|n| n == 0).unwrap_or(true),
        "upgraded stream stayed open"
    );
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            old_http.read(&mut buffer)
        )
        .await
        .unwrap()
        .unwrap(),
        0
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            worker_socket.read(&mut buffer)
        )
        .await
        .is_err(),
        "worker connection was disconnected"
    );
    assert_eq!(
        tls.get(format!("https://localhost:{worker}/v1/worker/workdirs"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.enabled",
        "false",
        "--global",
    ]);
    let status = server.get("/v1/status").await;
    assert_eq!(
        status["listeners"]["network"]["default"]["bound"],
        format!("0.0.0.0:{worker}")
    );
    // A bound status alone is insufficient: exercise the worker handler after
    // each independent change, with certificate verification still enabled.
    for (field, value) in [
        ("daemon.features.engine.enabled", "false"),
        (
            "daemon.network.listeners.loopback.transport",
            "loopback_http",
        ),
        ("daemon.network.listeners.loopback.enabled", "true"),
        (
            "daemon.network.listeners.loopback.allowedOrigins",
            "http://localhost:3000",
        ),
        ("daemon.features.engine.enabled", "true"),
    ] {
        assert_eq!(
            tls.get(format!("https://localhost:{worker}/v1/worker/workdirs"))
                .bearer_auth(&key)
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "worker stopped serving before {field}={value}"
        );
        home.cli_ok(&["config", "set", field, value, "--global"]);
        assert_eq!(
            tls.get(format!("https://localhost:{worker}/v1/worker/workdirs"))
                .bearer_auth(&key)
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "worker stopped serving after {field}={value}"
        );
    }
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            worker_socket.read(&mut buffer)
        )
        .await
        .is_err(),
        "worker socket closed during engine/origin changes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn default_off_and_two_line_recipe_are_idempotent_and_preserve_worker_settings() {
    let home = ApiHome::new();
    let server = home.start().await;
    let before = server.get("/v1/status").await;
    for name in ["default", "loopback"] {
        assert!(before["listeners"]["network"][name]["bound"].is_null());
    }
    // Use a private port for this test; the recipe itself never overwrites a
    // user's address or feature list. Address edits are restart-only.
    let local = port();
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.listen",
        &format!("127.0.0.1:{local}"),
        "--global",
    ]);
    drop(server);
    let server = home.start().await;
    for _ in 0..2 {
        home.cli_ok(&["config", "set", "daemon.features.engine.enabled", "true"]);
        home.cli_ok(&[
            "config",
            "set",
            "daemon.network.listeners.loopback.enabled",
            "true",
        ]);
        let status = wait_for_status(&server, "recipe enables loopback", |s| {
            s["listeners"]["network"]["loopback"]["bound"].is_string()
        })
        .await;
        assert_eq!(
            status["listeners"]["network"]["default"],
            before["listeners"]["network"]["default"]
        );
        assert_eq!(
            status["listeners"]["network"]["loopback"]["bound"],
            format!("127.0.0.1:{local}")
        );
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{local}/v1/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn bind_errors_are_independent_in_both_directions() {
    for failed in ["default", "loopback"] {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied = taken.local_addr().unwrap().port();
        let free = port();
        let home = ApiHome::new();
        home.write_config(json!({"daemon":{"network":{"listeners":{
            "default":{"enabled":true,"listen":format!("127.0.0.1:{}", if failed=="default" {occupied} else {free})},
            "loopback":{"enabled":true,"listen":format!("127.0.0.1:{}", if failed=="loopback" {occupied} else {free})}
        }}}}));
        let server = home.start().await;
        let good = if failed == "default" {
            "loopback"
        } else {
            "default"
        };
        let s = wait_for_status(&server, "one bound and one failure", |s| {
            s["listeners"]["network"][failed]["error"].is_string()
                && s["listeners"]["network"][good]["bound"].is_string()
        })
        .await;
        assert_eq!(s["status"], "ok");
        assert!(s["listeners"]["local"]["address"].is_string());
        assert!(s["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("named_listeners")));
        assert!(home.cli_ok(&["daemon", "status"]).contains(failed));
    }
}
#[test]
fn removed_network_keys_have_no_migration_or_alias() {
    let home = ApiHome::new();
    for (leaf, value) in [
        ("enabled", "true"),
        ("listen", "127.0.0.1:9000"),
        ("features", "worker"),
        ("tlsSans", "localhost"),
    ] {
        let error = home.cli_err(&[
            "config",
            "set",
            &format!("daemon.network.{leaf}"),
            value,
            "--global",
        ]);
        assert!(error.contains("unknown"), "{error}");
        assert!(!error.contains("moved"), "{error}");
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn browser_requests_never_reach_api_key_auth_and_origin_host_authorities_stay_separate() {
    let local = port();
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{"loopback":{"enabled":true,"listen":format!("127.0.0.1:{local}")}}}}}));
    let server = home.start().await;
    wait_for_status(&server, "loopback bound", |s| {
        s["listeners"]["network"]["loopback"]["bound"].is_string()
    })
    .await;
    let key = key(&home);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{local}");
    let health = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        health.json::<Value>().await.unwrap(),
        json!({"status":"ok"})
    );
    for origin in ["null", "https://evil.example", "http://localhost:3000"] {
        let response = client
            .get(format!("{base}/v1/health"))
            .header("Origin", origin)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));
    }
    let response = client
        .get(format!("{base}/v1/status"))
        .bearer_auth(&key)
        .header("Origin", "https://awman.prettysmart.dev")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://awman.prettysmart.dev"
    );
    let forged = client
        .get(format!("{base}/v1/health"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 421);
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.allowedOrigins",
        "http://localhost:3000",
        "--global",
    ]);
    let response = client
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/status"))
        .header("Origin", "http://localhost:3000")
        .header("Access-Control-Request-Private-Network", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(
        response.headers()["access-control-allow-private-network"],
        "true"
    );
    let response = client
        .get(format!("{base}/v1/health"))
        .header("Origin", "https://awman.prettysmart.dev")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_health_reports_listener_availability() {
    let local = port();
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{"loopback":{"enabled":true,"listen":format!("127.0.0.1:{local}")}}}}}));
    let server = home.start().await;
    wait_for_status(&server, "loopback bound", |s| {
        s["listeners"]["network"]["loopback"]["bound"].is_string()
    })
    .await;
    let url = format!("http://127.0.0.1:{local}/v1/health");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(&url)
        .header("Origin", "https://awman.prettysmart.dev")
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["browser_links"], true);
    assert_eq!(body["protocols"]["browser"], 1);
    assert_eq!(body["engine"]["state"], "on");
    assert!(body["capabilities"].as_array().is_some());

    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.features",
        "worker",
        "--global",
    ]);
    wait_for_status(&server, "engine removed from listener", |s| {
        !s["listeners"]["network"]["loopback"]["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "engine")
    })
    .await;
    let response = client
        .get(&url)
        .header("Origin", "https://awman.prettysmart.dev")
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["browser_links"], false);
    assert!(body["protocols"].get("browser").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn adversarial_loopback_requests_are_refused() {
    let local = port();
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{"loopback":{"enabled":true,"listen":format!("127.0.0.1:{local}")}}}}}));
    let server = home.start().await;
    wait_for_status(&server, "loopback bound", |s| {
        s["listeners"]["network"]["loopback"]["bound"].is_string()
    })
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{local}");
    let disallowed = client
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/status"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(disallowed.status(), 403);
    assert!(!disallowed
        .headers()
        .contains_key("access-control-allow-origin"));
    let form = client
        .post(format!("{base}/v1/engine/instances"))
        .header("Origin", "https://awman.prettysmart.dev")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("x=y")
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), 415);
    assert_eq!(form.headers()["referrer-policy"], "no-referrer");
    let wrong_host = client
        .get(format!("{base}/v1/health"))
        .header("Host", format!("localhost:{local}"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_host.status(), 421);

    let probe = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    if probe.connect("192.0.2.1:9").is_ok() {
        if let Ok(address) = probe.local_addr() {
            if !address.ip().is_loopback() {
                let destination = std::net::SocketAddr::new(address.ip(), local);
                assert!(std::net::TcpStream::connect_timeout(
                    &destination,
                    std::time::Duration::from_millis(250)
                )
                .is_err());
            }
        }
    }

    let mut raw = tokio::net::TcpStream::connect(("127.0.0.1", local))
        .await
        .unwrap();
    raw.write_all(format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{local}\r\nOrigin: https://awman.prettysmart.dev\r\nOrigin: https://awman.prettysmart.dev\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    raw.read_to_end(&mut bytes).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 403"));

    let mut raw = tokio::net::TcpStream::connect(("127.0.0.1", local))
        .await
        .unwrap();
    raw.write_all(
        format!(
            "GET /v1/../v1/keys HTTP/1.1\r\nHost: 127.0.0.1:{local}\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    raw.read_to_end(&mut bytes).await.unwrap();
    assert!(
        String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 404"),
        "{:#?}",
        String::from_utf8_lossy(&bytes)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_loopback_host_must_match_exact_listener_address() {
    let Ok(reservation) = std::net::TcpListener::bind("[::1]:0") else {
        return;
    };
    let local = reservation.local_addr().unwrap().port();
    drop(reservation);
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{"loopback":{"enabled":true,"listen":format!("[::1]:{local}")}}}}}));
    let server = home.start().await;
    wait_for_status(&server, "IPv6 loopback bound", |s| {
        s["listeners"]["network"]["loopback"]["bound"]
            .as_str()
            .is_some_and(|v| v.starts_with("[::1]:"))
    })
    .await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://[::1]:{local}/v1/health"))
        .header("Host", format!("localhost:{local}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 421);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_reload_and_later_listen_edits_leave_live_listeners_unchanged() {
    let local = port();
    let worker = port();
    let home = ApiHome::new();
    let config = json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{
        "default":{"enabled":true,"listen":format!("127.0.0.1:{worker}")},
        "loopback":{"enabled":true,"listen":format!("127.0.0.1:{local}")}
    }}}});
    home.write_config(config.clone());
    let server = home.start().await;
    let before = wait_for_status(&server, "both bound", |s| {
        s["listeners"]["network"]["default"]["bound"].is_string()
            && s["listeners"]["network"]["loopback"]["bound"].is_string()
    })
    .await;
    for listen in [format!("0.0.0.0:{local}"), format!("localhost:{local}")] {
        let error = home.cli_err(&[
            "config",
            "set",
            "daemon.network.listeners.loopback.listen",
            &listen,
            "--global",
        ]);
        assert!(error.contains("loopback"), "{error}");
    }
    let mut invalid = config;
    invalid["daemon"]["network"]["listeners"]["loopback"]["listen"] =
        json!(format!("0.0.0.0:{local}"));
    // Simulate an external editor to exercise reload validation separately.
    home.write_config(invalid);
    let error = home.cli_err(&["daemon", "api", "--raw", "POST", "/v1/config/reload"]);
    assert!(error.contains("loopback"), "{error}");
    assert_eq!(
        server.get("/v1/status").await["listeners"],
        before["listeners"]
    );
}

#[test]
fn config_set_edits_only_the_named_listener_and_show_includes_builtins() {
    let home = ApiHome::new();
    let output = home.cli_ok(&["config", "show"]);
    assert!(
        output.contains("daemon.network.listeners.default.listen"),
        "{output}"
    );
    assert!(
        output.contains("daemon.network.listeners.loopback.allowedOrigins"),
        "{output}"
    );
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.default.features",
        "squad,worker",
        "--global",
    ]);
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.allowedOrigins",
        "http://localhost:3000",
        "--global",
    ]);
    let output = home.cli_ok(&["config", "show", "--json"]);
    assert!(output.contains("squad"), "{output}");
    // An unset built-in leaf returns to its built-in origin, without touching default.
    home.cli_ok(&[
        "config",
        "unset",
        "daemon.network.listeners.loopback.allowedOrigins",
        "--global",
    ]);
    let output = home.cli_ok(&["config", "show"]);
    assert!(output.contains("https://awman.prettysmart.dev"), "{output}");
    assert!(output.contains("squad"), "{output}");
}
