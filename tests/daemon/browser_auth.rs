//! Browser pairing against the shipped daemon (WI 0159): a real loopback
//! `engine` listener, the owner socket and `awman daemon api browsers.*`.
use crate::{api_process::ApiHome, support::*};
use serde_json::{json, Value};

const ORIGIN: &str = "https://awman.prettysmart.dev";

fn port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn loopback_daemon() -> (ApiHome, crate::api_process::ApiServer, String) {
    let local = port();
    let home = ApiHome::new();
    home.write_config(
        json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{
            "loopback":{"enabled":true,"listen":format!("127.0.0.1:{local}")}
        }}}}),
    );
    let server = home.start().await;
    wait_for_status(&server, "loopback bound", |s| {
        s["listeners"]["network"]["loopback"]["bound"].is_string()
    })
    .await;
    (home, server, format!("http://127.0.0.1:{local}"))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn pair(server: &crate::api_process::ApiServer, base: &str) -> Value {
    let http = client();
    let created: Value = http
        .post(format!("{base}/v1/browser-links"))
        .header("Origin", ORIGIN)
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = created["code"].as_str().expect("code");
    let (status, view) = http_owner(
        server,
        reqwest::Method::GET,
        &format!("/v1/browser-links?code={code}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["listener"], "loopback");
    let (status, _) = http_owner(
        server,
        reqwest::Method::POST,
        &format!(
            "/v1/browser-links/{}/decision",
            view["id"].as_str().unwrap()
        ),
        Some(json!({"approve": true})),
    )
    .await;
    assert_eq!(status, 200);
    let polled: Value = http
        .post(format!(
            "{base}/v1/browser-links/{}/poll",
            created["id"].as_str().unwrap()
        ))
        .header("Origin", ORIGIN)
        .header("Content-Type", "application/json")
        .body(json!({"polling_secret": created["polling_secret"]}).to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(polled["status"], "approved", "{polled}");
    polled["grant"].clone()
}

async fn http_owner(
    server: &crate::api_process::ApiServer,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    http(server, method, path, None, body).await
}

async fn with_grant(base: &str, path: &str, grant: &Value) -> (u16, Value) {
    let response = client()
        .get(format!("{base}{path}"))
        .header("Origin", ORIGIN)
        .bearer_auth(grant["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_link_grant_status_and_revocation_through_daemon_api() {
    let (home, server, base) = loopback_daemon().await;
    let grant = pair(&server, &base).await;
    let (status, _) = with_grant(&base, "/v1/status", &grant).await;
    assert_eq!(status, 200);
    let (status, _) = with_grant(&base, "/v1/worker/workdirs", &grant).await;
    assert_eq!(status, 404);
    // `daemon status` counts the live grant on its listener.
    wait_for_status(&server, "grant counted", |s| {
        s["listeners"]["network"]["loopback"]["browser_grants"] == 1
    })
    .await;
    let listed: Value =
        serde_json::from_str(&home.cli_ok(&["daemon", "api", "browsers.list", "--json"])).unwrap();
    let items = listed["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["origin"], ORIGIN);
    assert!(!listed
        .to_string()
        .contains(grant["token"].as_str().unwrap()));
    // The grant file is owner-private and holds no token.
    let file = home
        .home()
        .join(".local/share/awman/data/daemon/browser_grants.json");
    let disk = std::fs::read_to_string(&file).unwrap();
    assert!(!disk.contains(grant["token"].as_str().unwrap()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    home.cli_ok(&[
        "daemon",
        "api",
        "browsers.delete",
        &format!("id={}", grant["id"].as_str().unwrap()),
    ]);
    let (status, body) = with_grant(&base, "/v1/status", &grant).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (401, Some("browser_grant_invalid"))
    );
    // Malformed ids are refused locally by the action table.
    home.cli_err(&["daemon", "api", "browsers.delete", "id=key_123"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_origin_revokes_its_grants() {
    let (home, server, base) = loopback_daemon().await;
    let grant = pair(&server, &base).await;
    home.cli_ok(&[
        "config",
        "set",
        "daemon.network.listeners.loopback.allowedOrigins",
        "http://localhost:3000",
        "--global",
    ]);
    // The origin is gone from the listener: the grant is revoked and the
    // origin itself is refused at the gate.
    wait_for_status(&server, "grant revoked", |s| {
        s["listeners"]["network"]["loopback"]["browser_grants"] == 0
    })
    .await;
    let (status, _) = with_grant(&base, "/v1/status", &grant).await;
    assert_eq!(status, 403);
}

/// L-7: the same-machine boundary. A `tls` listener with allowed origins
/// never pairs and never accepts a grant.
#[tokio::test(flavor = "multi_thread")]
async fn grants_are_refused_on_non_loopback_listeners() {
    let local = port();
    let worker = port();
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"features":{"engine":{"enabled":true},"worker":{"enabled":true}},"network":{"listeners":{
        "loopback":{"enabled":true,"listen":format!("127.0.0.1:{local}")},
        "default":{"enabled":true,"listen":format!("127.0.0.1:{worker}"),"allowedOrigins":[ORIGIN]}
    }}}}));
    let server = home.start().await;
    wait_for_status(&server, "both bound", |s| {
        s["listeners"]["network"]["loopback"]["bound"].is_string()
            && s["listeners"]["network"]["default"]["bound"].is_string()
    })
    .await;
    let grant = pair(&server, &format!("http://127.0.0.1:{local}")).await;
    let cert = std::fs::read(
        home.home()
            .join(".local/share/awman/data/daemon/tls/cert.pem"),
    )
    .unwrap();
    let tls = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let base = format!("https://localhost:{worker}");
    for (method, path) in [
        (reqwest::Method::POST, "/v1/browser-links"),
        (reqwest::Method::GET, "/v1/status"),
        (reqwest::Method::GET, "/v1/worker/workdirs"),
    ] {
        let response = tls
            .request(method, format!("{base}{path}"))
            .header("Origin", ORIGIN)
            .header("Content-Type", "application/json")
            .bearer_auth(grant["token"].as_str().unwrap())
            .send()
            .await
            .unwrap();
        assert!(
            [403, 404].contains(&response.status().as_u16()),
            "{path}: {}",
            response.status()
        );
        assert!(response
            .headers()
            .get("access-control-allow-origin")
            .is_none());
    }
    // Without an Origin the grant is not an API key either.
    let response = tls
        .get(format!("{base}/v1/worker/workdirs"))
        .bearer_auth(grant["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}
