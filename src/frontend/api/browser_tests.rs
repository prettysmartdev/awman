//! Browser pairing over real HTTP and WebSockets against the in-process
//! router (WI 0159): a loopback `engine` listener plus the owner socket
//! router, sharing one `DaemonApi`.

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::command::daemon_api::DaemonApi;
use crate::command::daemon_runtime::network::{BrowserRevocation, BrowserRevocationSink};
use crate::frontend::api::routes::{build_router_for_listener, ListenerPolicy};

const ORIGIN: &str = "https://awman.prettysmart.dev";
const DEV: &str = "http://localhost:3000";

struct Daemon {
    _tmp: tempfile::TempDir,
    api: Arc<DaemonApi>,
    /// The loopback browser listener.
    net: SocketAddr,
    /// The owner socket router (served over TCP here for the test).
    owner: SocketAddr,
    /// A `tls` listener with the same origins (served as plain HTTP in-process;
    /// its transport setting is what refuses browsers).
    tls: SocketAddr,
    http: reqwest::Client,
}

fn config(
    home: &std::path::Path,
    net: SocketAddr,
    tls: SocketAddr,
    features: &[&str],
) -> crate::data::config::EffectiveConfig {
    use crate::data::config::{ConfigFile, ConfigLayers, EffectiveConfig, FileKind};
    EffectiveConfig::resolve(
        ConfigLayers {
            global: ConfigFile::from_value(
                json!({"daemon": {
                    "features": {"engine": {"enabled": true}, "worker": {"enabled": true}},
                    "network": {"listeners": {
                        "loopback": {"enabled": true, "listen": net.to_string(),
                            "features": features, "allowedOrigins": [ORIGIN, DEV]},
                        "default": {"enabled": true, "listen": format!("0.0.0.0:{}", tls.port()),
                            "transport": "tls", "features": ["worker"],
                            "allowedOrigins": [ORIGIN]}
                    }}
                }}),
                FileKind::Global,
            )
            .unwrap(),
            ..Default::default()
        },
        Default::default(),
        crate::data::config::env::EnvSnapshot::with_overrides([(
            "AWMAN_HOME",
            home.to_string_lossy().as_ref(),
        )]),
    )
    .unwrap()
}

async fn daemon_with(features: &[&str]) -> Daemon {
    daemon_with_hook(features, None).await
}

async fn daemon_with_hook(
    features: &[&str],
    hook: Option<super::super::routes::BrowserAuthenticatedHook>,
) -> Daemon {
    let tmp = tempfile::tempdir().unwrap();
    let net_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let net = net_listener.local_addr().unwrap();
    let tls = tls_listener.local_addr().unwrap();
    let owner = owner_listener.local_addr().unwrap();
    let state = crate::frontend::api::routes::tests::make_test_state(tmp.path());
    *state.live_config.write().unwrap() = Some(config(tmp.path(), net, tls, features));
    let api = Arc::new(DaemonApi::for_worker_tests(state));
    api.runtime.enable_engine_for_tests().await;
    for (listener, mut router) in [
        (
            net_listener,
            build_router_for_listener(
                api.clone(),
                ListenerPolicy::Network {
                    name: "loopback".into(),
                },
            ),
        ),
        (
            tls_listener,
            build_router_for_listener(
                api.clone(),
                ListenerPolicy::Network {
                    name: "default".into(),
                },
            ),
        ),
        (
            owner_listener,
            build_router_for_listener(api.clone(), ListenerPolicy::Local).layer(axum::Extension(
                crate::frontend::api::local::OwnerAuthenticated,
            )),
        ),
    ] {
        if let Some(hook) = &hook {
            router = router.layer(axum::Extension(hook.clone()));
        }
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }
    Daemon {
        _tmp: tmp,
        api,
        net,
        owner,
        tls,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
    }
}

async fn daemon() -> Daemon {
    daemon_with(&["engine"]).await
}

async fn body(response: reqwest::Response) -> (u16, reqwest::header::HeaderMap, Value) {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    (
        status,
        headers,
        response.json().await.unwrap_or(Value::Null),
    )
}

impl Daemon {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.net)
    }

    /// A browser `fetch` from `origin`: Origin, JSON content type.
    fn browser(
        &self,
        method: reqwest::Method,
        path: &str,
        origin: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .header("Origin", origin)
            .header("Sec-Fetch-Mode", "cors")
            .header("Content-Type", "application/json")
    }

    async fn create_link(&self, origin: &str) -> (u16, reqwest::header::HeaderMap, Value) {
        body(
            self.browser(reqwest::Method::POST, "/v1/browser-links", origin)
                .body("{}")
                .send()
                .await
                .unwrap(),
        )
        .await
    }

    async fn poll(
        &self,
        id: &str,
        secret: &str,
        origin: &str,
    ) -> (u16, reqwest::header::HeaderMap, Value) {
        body(
            self.browser(
                reqwest::Method::POST,
                &format!("/v1/browser-links/{id}/poll"),
                origin,
            )
            .body(json!({"polling_secret": secret}).to_string())
            .send()
            .await
            .unwrap(),
        )
        .await
    }

    async fn owner(
        &self,
        method: reqwest::Method,
        path: &str,
        json: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.owner));
        if let Some(json) = json {
            request = request.json(&json);
        }
        let (status, _, value) = body(request.send().await.unwrap()).await;
        (status, value)
    }

    /// Pair a browser from `origin`, approving on the owner side.
    async fn grant(&self, origin: &str) -> Value {
        let (status, _, created) = self.create_link(origin).await;
        assert_eq!(status, 201, "{created}");
        let code = created["code"].as_str().unwrap();
        let (status, view) = self
            .owner(
                reqwest::Method::GET,
                &format!("/v1/browser-links?code={code}"),
                None,
            )
            .await;
        assert_eq!(status, 200, "{view}");
        let (status, _) = self
            .owner(
                reqwest::Method::POST,
                &format!(
                    "/v1/browser-links/{}/decision",
                    view["id"].as_str().unwrap()
                ),
                Some(json!({"approve": true})),
            )
            .await;
        assert_eq!(status, 200);
        let (status, _, polled) = self
            .poll(
                created["id"].as_str().unwrap(),
                created["polling_secret"].as_str().unwrap(),
                origin,
            )
            .await;
        assert_eq!(status, 200, "{polled}");
        assert_eq!(polled["status"], "approved");
        polled["grant"].clone()
    }

    async fn with_grant(
        &self,
        method: reqwest::Method,
        path: &str,
        grant: &Value,
        origin: &str,
    ) -> (u16, reqwest::header::HeaderMap, Value) {
        body(
            self.browser(method, path, origin)
                .bearer_auth(grant["token"].as_str().unwrap())
                .send()
                .await
                .unwrap(),
        )
        .await
    }

    async fn ticket(&self, grant: &Value, request: Value) -> (u16, Value) {
        let (status, _, value) = body(
            self.browser(reqwest::Method::POST, "/v1/browser-tickets", ORIGIN)
                .bearer_auth(grant["token"].as_str().unwrap())
                .body(request.to_string())
                .send()
                .await
                .unwrap(),
        )
        .await;
        (status, value)
    }

    async fn browser_instance(&self, grant: &Value) -> String {
        let (status, _, created) = body(
            self.browser(reqwest::Method::POST, "/v1/engine/instances", ORIGIN)
                .bearer_auth(grant["token"].as_str().unwrap())
                .body("{}")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 201, "{created}");
        created["id"].as_str().unwrap().to_owned()
    }

    /// Open a WebSocket the way a browser does: Origin plus offered
    /// subprotocols, no Authorization header.
    async fn websocket(
        &self,
        path: &str,
        protocols: &str,
    ) -> Result<
        (
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::handshake::client::Response,
        ),
        u16,
    > {
        let mut request = format!("ws://{}{path}", self.net)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Origin", ORIGIN.parse().unwrap());
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", protocols.parse().unwrap());
        match tokio_tungstenite::connect_async(request).await {
            Ok(ok) => Ok(ok),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(other) => panic!("{other}"),
        }
    }
}

async fn next_close(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> (u16, String) {
    let deadline = tokio::time::Duration::from_secs(5);
    loop {
        match tokio::time::timeout(deadline, ws.next())
            .await
            .expect("close in time")
        {
            Some(Ok(WsMessage::Close(Some(frame)))) => {
                return (frame.code.into(), frame.reason.to_string())
            }
            Some(Ok(_)) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn link_flow_issues_an_origin_and_listener_bound_engine_grant_once() {
    let d = daemon().await;
    let (status, headers, created) = d.create_link(ORIGIN).await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["access-control-allow-origin"], ORIGIN);
    assert!(headers.get("access-control-allow-credentials").is_none());
    assert!(headers.get("set-cookie").is_none());
    let code = created["code"].as_str().unwrap();
    assert!(code.starts_with("web-") && code.len() == 13, "{code}");
    assert!(created["id"].as_str().unwrap().starts_with("blk_"));
    assert_eq!(created["polling_secret"].as_str().unwrap().len(), 43);
    assert_eq!(created["interval_ms"], 2000);
    assert_eq!(created["scope"], "engine");
    assert_eq!(created["listener"], "loopback");
    assert_eq!(created["origin"], ORIGIN);
    let expires = chrono::DateTime::parse_from_rfc3339(created["expires_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let ttl = expires - chrono::Utc::now();
    assert!(ttl <= chrono::Duration::seconds(300) && ttl > chrono::Duration::seconds(290));

    // Pending until the owner decides.
    let id = created["id"].as_str().unwrap();
    let secret = created["polling_secret"].as_str().unwrap();
    let (status, _, pending) = d.poll(id, secret, ORIGIN).await;
    assert_eq!(status, 200);
    assert_eq!(pending, json!({"status":"pending","retry_after_ms":2000}));

    // Owner lookup accepts lower case; shows origin, listener, scope.
    let (status, view) = d
        .owner(
            reqwest::Method::GET,
            &format!("/v1/browser-links?code={}", code.to_lowercase()),
            None,
        )
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["origin"], ORIGIN);
    assert_eq!(view["listener"], "loopback");
    assert_eq!(view["scope"], "engine");
    assert_eq!(view["status"], "pending");
    let (status, decided) = d
        .owner(
            reqwest::Method::POST,
            &format!("/v1/browser-links/{id}/decision"),
            Some(json!({"approve": true})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(decided["status"], "approved");
    // A decided code is no longer looked up; a second decision conflicts.
    let (status, _) = d
        .owner(
            reqwest::Method::GET,
            &format!("/v1/browser-links?code={code}"),
            None,
        )
        .await;
    assert_eq!(status, 404);
    let (status, conflict) = d
        .owner(
            reqwest::Method::POST,
            &format!("/v1/browser-links/{id}/decision"),
            Some(json!({"approve": false})),
        )
        .await;
    assert_eq!((status, conflict["code"].as_str()), (409, Some("conflict")));

    // The first poll after approval is spaced by the interval.
    tokio::time::sleep(std::time::Duration::from_millis(1900)).await;
    let (status, _, approved) = d.poll(id, secret, ORIGIN).await;
    assert_eq!(status, 200, "{approved}");
    let grant = approved["grant"].clone();
    assert_eq!(approved["status"], "approved");
    assert_eq!(grant["id"], decided["grant_id"]);
    assert!(grant["token"].as_str().unwrap().starts_with("awb_"));
    assert_eq!(grant["scope"], "engine");
    assert_eq!(grant["origin"], ORIGIN);
    assert_eq!(grant["listener"], "loopback");
    // One-use: the second poll is gone.
    let (status, _, gone) = d.poll(id, secret, ORIGIN).await;
    assert_eq!((status, gone["code"].as_str()), (410, Some("link_gone")));

    // The grant reaches engine and status routes, not worker or owner ones.
    let (status, headers, _) = d
        .with_grant(reqwest::Method::GET, "/v1/status", &grant, ORIGIN)
        .await;
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "no-store");
    let (status, _, _) = d
        .with_grant(reqwest::Method::GET, "/v1/engine/instances", &grant, ORIGIN)
        .await;
    assert_eq!(status, 200);
    for path in [
        "/v1/worker/workdirs",
        "/v1/keys",
        "/v1/browsers",
        "/v1/config",
    ] {
        let (status, _, _) = d
            .with_grant(reqwest::Method::GET, path, &grant, ORIGIN)
            .await;
        assert_eq!(status, 404, "{path}");
    }
    // The grant is listed for the owner (never the token or hash) and counted.
    let (status, list) = d.owner(reqwest::Method::GET, "/v1/browsers", None).await;
    assert_eq!(status, 200);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert!(list
        .to_string()
        .find(grant["token"].as_str().unwrap())
        .is_none());
    assert!(list["items"][0].get("hash").is_none());
    // `daemon status` reports the live grant count per listener.
    assert_eq!(
        d.api.runtime.listeners().network["loopback"].browser_grants,
        1
    );
    assert_eq!(
        d.api.runtime.browser_auth().count_for_listener("loopback"),
        1
    );
    // The grant file holds the hash only.
    let disk = std::fs::read_to_string(d.api.runtime.browser_auth().grants().path()).unwrap();
    assert!(!disk.contains(grant["token"].as_str().unwrap()));

    // Revocation: 204, then the grant is refused.
    let (status, _) = d
        .owner(
            reqwest::Method::DELETE,
            &format!("/v1/browsers/{}", grant["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (status, _, refused) = d
        .with_grant(reqwest::Method::GET, "/v1/status", &grant, ORIGIN)
        .await;
    assert_eq!(
        (status, refused["code"].as_str()),
        (401, Some("browser_grant_invalid"))
    );
    let (status, missing) = d
        .owner(
            reqwest::Method::DELETE,
            &format!("/v1/browsers/{}", grant["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(
        (status, missing["code"].as_str()),
        (404, Some("grant_not_found"))
    );
}

#[tokio::test]
async fn denied_links_report_denied_once_then_gone() {
    let d = daemon().await;
    let (_, _, created) = d.create_link(ORIGIN).await;
    let id = created["id"].as_str().unwrap();
    let (status, decided) = d
        .owner(
            reqwest::Method::POST,
            &format!("/v1/browser-links/{id}/decision"),
            Some(json!({"approve": false})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(decided, json!({"status":"denied"}));
    let secret = created["polling_secret"].as_str().unwrap();
    let (status, _, denied) = d.poll(id, secret, ORIGIN).await;
    assert_eq!((status, denied), (200, json!({"status":"denied"})));
    let (status, _, _) = d.poll(id, secret, ORIGIN).await;
    assert_eq!(status, 410);
    assert!(d
        .api
        .runtime
        .browser_auth()
        .grants()
        .list(chrono::Utc::now())
        .is_empty());
}

#[tokio::test]
async fn wrong_missing_and_null_origins_and_forged_hosts_are_refused_without_cors() {
    let d = daemon().await;
    for origin in [
        "null",
        "https://evil.example",
        "http://127.0.0.1:3000",
        "https://awman.prettysmart.dev/",
    ] {
        let (status, headers, _) = d.create_link(origin).await;
        assert_eq!(status, 403, "{origin}");
        assert!(
            headers.get("access-control-allow-origin").is_none(),
            "{origin}"
        );
    }
    // No Origin at all: not a browser request, so no pairing (API-key path).
    let response = d
        .http
        .post(d.url("/v1/browser-links"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    // DNS rebinding: a Host other than the literal listener address.
    for host in [
        "evil.example",
        "localhost",
        &format!("localhost:{}", d.net.port()),
    ] {
        let response = d
            .browser(reqwest::Method::POST, "/v1/browser-links", ORIGIN)
            .header("Host", host)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 421, "{host}");
    }
}

#[tokio::test]
async fn preflights_leak_nothing_to_disallowed_origins() {
    let d = daemon().await;
    let refused = d
        .http
        .request(reqwest::Method::OPTIONS, d.url("/v1/browser-links"))
        .header("Origin", "https://evil.example")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Private-Network", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 403);
    for header in [
        "access-control-allow-origin",
        "access-control-allow-methods",
        "access-control-allow-headers",
        "access-control-allow-private-network",
    ] {
        assert!(refused.headers().get(header).is_none(), "{header}");
    }
    let allowed = d
        .http
        .request(reqwest::Method::OPTIONS, d.url("/v1/browser-links"))
        .header("Origin", DEV)
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 204);
    assert_eq!(allowed.headers()["access-control-allow-origin"], DEV);
    assert_ne!(allowed.headers()["access-control-allow-origin"], "*");
    assert!(allowed
        .headers()
        .get("access-control-allow-credentials")
        .is_none());
}

#[tokio::test]
async fn cross_site_form_posts_are_refused() {
    let d = daemon().await;
    for content_type in [
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "text/plain",
    ] {
        let response = d
            .http
            .post(d.url("/v1/browser-links"))
            .header("Origin", ORIGIN)
            .header("Content-Type", content_type)
            .body("a=b")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 415, "{content_type}");
    }
    // Same for an evil origin: refused before content negotiation.
    let response = d
        .http
        .post(d.url("/v1/browser-links"))
        .header("Origin", "https://evil.example")
        .header("Content-Type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    // Fields in the create body (e.g. asking for a scope) are refused.
    let response = d
        .browser(reqwest::Method::POST, "/v1/browser-links", ORIGIN)
        .body(json!({"scope":"owner"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn wrong_origin_wrong_secret_and_reused_polls_are_gone_then_rate_limited() {
    let d = daemon().await;
    let (_, _, created) = d.create_link(ORIGIN).await;
    let id = created["id"].as_str().unwrap();
    let secret = created["polling_secret"].as_str().unwrap();
    // Another allowed origin cannot poll this link (D5), nor can a wrong or
    // malformed secret or id. All are the same 410.
    let mut failures = 0;
    for (id, secret, origin) in [
        (id, secret, DEV),
        (id, "wrong", ORIGIN),
        ("blk_00000000000000000000000000000000", secret, ORIGIN),
        ("not-an-id", secret, ORIGIN),
    ] {
        let (status, _, body) = d.poll(id, secret, origin).await;
        assert_eq!((status, body["code"].as_str()), (410, Some("link_gone")));
        if origin == ORIGIN {
            failures += 1;
        }
    }
    // A missing secret is a malformed request.
    let response = d
        .browser(
            reqwest::Method::POST,
            &format!("/v1/browser-links/{id}/poll"),
            ORIGIN,
        )
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    // The right secret still works: failures did not consume the link.
    let (status, _, pending) = d.poll(id, secret, ORIGIN).await;
    assert_eq!((status, pending["status"].as_str()), (200, Some("pending")));
    // Polling faster than the interval: 429 with Retry-After, not consumed.
    let (status, headers, limited) = d.poll(id, secret, ORIGIN).await;
    assert_eq!(
        (status, limited["code"].as_str()),
        (429, Some("rate_limited"))
    );
    assert_eq!(headers["retry-after"], "2");
    assert_eq!(
        headers["access-control-expose-headers"],
        "Retry-After, ETag"
    );
    // Ten failures in a minute lock the origin out of polling.
    for _ in failures..10 {
        d.poll(id, "wrong", ORIGIN).await;
    }
    let (status, headers, _) = d.poll(id, secret, ORIGIN).await;
    assert_eq!(status, 429);
    assert!(
        headers["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 50
    );
    // The other origin has its own budget.
    let (status, _, _) = d.poll(id, secret, DEV).await;
    assert_eq!(status, 410);
}

#[tokio::test]
async fn link_creation_and_owner_lookups_are_rate_limited() {
    let d = daemon().await;
    for _ in 0..3 {
        let (status, _, _) = d.create_link(ORIGIN).await;
        assert_eq!(status, 201);
    }
    let (status, headers, limited) = d.create_link(ORIGIN).await;
    assert_eq!(
        (status, limited["code"].as_str()),
        (429, Some("rate_limited"))
    );
    assert!(headers.contains_key("retry-after"));
    for _ in 0..5 {
        let (status, body) = d
            .owner(
                reqwest::Method::GET,
                "/v1/browser-links?code=web-AAAA-AAAA",
                None,
            )
            .await;
        assert_eq!(
            (status, body["code"].as_str()),
            (404, Some("link_not_found"))
        );
    }
    let (status, _) = d
        .owner(
            reqwest::Method::GET,
            "/v1/browser-links?code=web-AAAA-AAAA",
            None,
        )
        .await;
    assert_eq!(status, 429);
}

#[tokio::test]
async fn api_keys_and_foreign_bearers_never_authenticate_browser_routes() {
    let d = daemon().await;
    let (_, key) = d.api.runtime.keys().create("ci").unwrap();
    let grant = d.grant(ORIGIN).await;
    for (bearer, code) in [
        (Some(key.as_str().to_owned()), "browser_grant_invalid"),
        // A work-server refresh token or any other opaque credential.
        (
            Some("wsr_0123456789abcdef".to_owned()),
            "browser_grant_invalid",
        ),
        (
            Some(format!("{}x", grant["token"].as_str().unwrap())),
            "browser_grant_invalid",
        ),
        (None, "browser_grant_required"),
    ] {
        let mut request = d.browser(reqwest::Method::GET, "/v1/status", ORIGIN);
        if let Some(bearer) = &bearer {
            request = request.bearer_auth(bearer);
        }
        let (status, headers, body) = body(request.send().await.unwrap()).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (401, Some(code)),
            "{bearer:?}"
        );
        assert_eq!(headers["access-control-allow-origin"], ORIGIN);
    }
    // The grant is bound to its origin: the other allowed origin is refused.
    let (status, _, refused) = d
        .with_grant(reqwest::Method::GET, "/v1/status", &grant, DEV)
        .await;
    assert_eq!(
        (status, refused["code"].as_str()),
        (401, Some("browser_grant_invalid"))
    );
    // Without an Origin, a grant is not an API key.
    let response = d
        .http
        .get(d.url("/v1/status"))
        .bearer_auth(grant["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    // API keys still work for non-browser clients, but not on pairing routes.
    let response = d
        .http
        .get(d.url("/v1/engine/instances"))
        .bearer_auth(key.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    for path in ["/v1/browser-links", "/v1/browser-tickets"] {
        let response = d
            .http
            .post(d.url(path))
            .bearer_auth(key.as_str())
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
}

#[tokio::test]
async fn grants_are_refused_on_non_loopback_and_non_engine_listeners() {
    let d = daemon().await;
    let grant = d.grant(ORIGIN).await;
    // The tls listener lists the origin but never serves browsers; engine
    // (and so pairing) cannot even be served there.
    let tls = format!("http://{}", d.tls);
    let response = d
        .http
        .post(format!("{tls}/v1/browser-links"))
        .header("Origin", ORIGIN)
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let response = d
        .http
        .get(format!("{tls}/v1/status"))
        .header("Origin", ORIGIN)
        .bearer_auth(grant["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let response = d
        .http
        .get(format!("{tls}/v1/status"))
        .bearer_auth(grant["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // A loopback listener that does not serve engine offers no pairing.
    let d = daemon_with(&["worker"]).await;
    let (status, _, _) = d.create_link(ORIGIN).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn browsers_never_see_terminal_users_instances() {
    let d = daemon().await;
    let grant = d.grant(ORIGIN).await;
    // An instance the owner (a TUI) created is invisible to the browser.
    let (status, created) = d
        .owner(
            reqwest::Method::POST,
            "/v1/engine/instances",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 201);
    let tui = created["id"].as_str().unwrap().to_owned();
    let mine = d.browser_instance(&grant).await;
    let (status, _, list) = d
        .with_grant(reqwest::Method::GET, "/v1/engine/instances", &grant, ORIGIN)
        .await;
    assert_eq!(status, 200);
    let ids: Vec<_> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec![mine.clone()]);
    for path in [
        format!("/v1/engine/instances/{tui}"),
        format!("/v1/engine/instances/{tui}/tabs"),
        format!("/v1/engine/instances/{tui}/dialogs"),
    ] {
        let (status, _, _) = d
            .with_grant(reqwest::Method::GET, &path, &grant, ORIGIN)
            .await;
        assert_eq!(status, 404, "{path}");
    }
    let (status, _, _) = d
        .with_grant(
            reqwest::Method::GET,
            &format!("/v1/engine/instances/{mine}"),
            &grant,
            ORIGIN,
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = d
        .ticket(&grant, json!({"route":"frontend","instance_id": tui}))
        .await;
    assert_eq!(status, 404);
    // A second browser grant does not see the first one's instance either.
    let other = d.grant(ORIGIN).await;
    let (status, _, _) = d
        .with_grant(
            reqwest::Method::GET,
            &format!("/v1/engine/instances/{mine}"),
            &other,
            ORIGIN,
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn head_requests_enforce_grants_and_instance_visibility() {
    let d = daemon().await;
    let grant = d.grant(ORIGIN).await;
    let (_, hidden) = d
        .owner(
            reqwest::Method::POST,
            "/v1/engine/instances",
            Some(json!({})),
        )
        .await;
    let hidden_path = format!("/v1/engine/instances/{}", hidden["id"].as_str().unwrap());
    for path in [
        "/v1/status",
        "/v1/engine/catalogue",
        "/v1/engine/instances",
        &hidden_path,
    ] {
        let response = d
            .browser(reqwest::Method::HEAD, path, ORIGIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "anonymous HEAD {path}");
        let response = d
            .browser(reqwest::Method::HEAD, path, ORIGIN)
            .bearer_auth("awb_invalid")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "invalid HEAD {path}");
    }
    let (status, _, _) = d
        .with_grant(reqwest::Method::HEAD, &hidden_path, &grant, ORIGIN)
        .await;
    assert_eq!(status, 404);
    let (status, _, _) = d
        .with_grant(reqwest::Method::HEAD, "/v1/status", &grant, ORIGIN)
        .await;
    assert_eq!(status, 200);
    d.owner(
        reqwest::Method::DELETE,
        &format!("/v1/browsers/{}", grant["id"].as_str().unwrap()),
        None,
    )
    .await;
    let (status, _, _) = d
        .with_grant(reqwest::Method::HEAD, "/v1/status", &grant, ORIGIN)
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn url_credentials_are_rejected_on_all_methods_and_gate_routes() {
    let d = daemon().await;
    let grant = d.grant(ORIGIN).await;
    for query in [
        "%74icket=secret",
        "ticket=secret",
        "ToKeN=secret",
        "a=1&%61ccess%5ftoken=secret",
        "ticket=&ticket=secret",
        "%54%4f%4b%45%4e=secret",
    ] {
        for path in ["/v1/status", "/v1/health", "/unknown"] {
            for method in [
                reqwest::Method::GET,
                reqwest::Method::HEAD,
                reqwest::Method::OPTIONS,
                reqwest::Method::PUT,
            ] {
                let url = format!("{path}?{query}");
                let response = d
                    .browser(method.clone(), &url, ORIGIN)
                    .bearer_auth(grant["token"].as_str().unwrap())
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 400, "{method} {url}");
                assert_eq!(response.headers()["access-control-allow-origin"], ORIGIN);
                let evil = d
                    .browser(method, &url, "https://evil.example")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(evil.status(), 403);
                assert!(!evil.headers().contains_key("access-control-allow-origin"));
            }
        }
    }
    // Ordinary query values and lookalike names are still usable.
    let (status, _, _) = d
        .with_grant(
            reqwest::Method::GET,
            "/v1/status?note=token&ticket_count=1",
            &grant,
            ORIGIN,
        )
        .await;
    assert_eq!(status, 200);
}

#[derive(Clone, Default)]
struct TraceSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for TraceSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TraceSink {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn rejected_url_credentials_never_enter_http_trace_spans() {
    use hyper::service::Service;
    use tracing::instrument::WithSubscriber;
    let d = daemon().await;
    for level in [tracing::Level::DEBUG, tracing::Level::TRACE] {
        let sink = TraceSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(level)
            .finish();
        async {
            for path in [
                "/v1/status?ticket=awt_TRACE_SENTINEL",
                "/v1/status?%74oken=awb_TRACE_SENTINEL",
                "/v1/health?token=awb_HEALTH_SENTINEL",
            ] {
                let request = axum::http::Request::builder()
                    .uri(path)
                    .header("host", d.net.to_string())
                    .header("origin", ORIGIN)
                    .body(axum::body::Body::empty())
                    .unwrap();
                let router = build_router_for_listener(
                    d.api.clone(),
                    ListenerPolicy::Network {
                        name: "loopback".into(),
                    },
                );
                let response = hyper_util::service::TowerToHyperService::new(router)
                    .call(request)
                    .await
                    .unwrap();
                // Drain to exercise both start and end tracing events.
                axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
            }
        }
        .with_subscriber(subscriber)
        .await;
        let bytes = sink.0.lock().unwrap();
        let log = String::from_utf8_lossy(&bytes);
        assert!(
            log.contains("request"),
            "capture must contain HTTP diagnostics"
        );
        assert!(
            !log.contains("SENTINEL"),
            "credential leaked at {level}: {log}"
        );
    }
}

#[tokio::test]
async fn duplicate_hosts_are_rejected_even_when_the_first_matches() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let d = daemon().await;
    for second in [d.net.to_string(), "evil.example".into()] {
        let mut stream = tokio::net::TcpStream::connect(d.net).await.unwrap();
        stream.write_all(format!("GET /v1/status HTTP/1.1\r\nHost: {}\r\nHost: {second}\r\nOrigin: {ORIGIN}\r\nConnection: close\r\n\r\n", d.net).as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 421"), "{response}");
        assert!(!response
            .to_ascii_lowercase()
            .contains("access-control-allow-origin"));
    }
}

#[tokio::test]
async fn tickets_are_one_use_bound_and_never_echoed() {
    let d = daemon().await;
    let grant = d.grant(ORIGIN).await;
    let instance = d.browser_instance(&grant).await;
    let other = d.browser_instance(&grant).await;
    let path = format!("/v1/engine/instances/{instance}/frontend");

    let (status, ticket) = d
        .ticket(&grant, json!({"route":"frontend","instance_id": instance}))
        .await;
    assert_eq!(status, 201, "{ticket}");
    assert_eq!(ticket["expires_in_ms"], 30000);
    let t = ticket["ticket"].as_str().unwrap().to_owned();
    assert!(t.starts_with("awt_") && t.len() == 47);

    // Credentials in the URL are refused outright.
    let refused = d
        .websocket(
            &format!("{path}?ticket={t}"),
            &format!("awman.frontend.v1, awman.ticket.{t}"),
        )
        .await
        .err();
    assert_eq!(refused, Some(400));

    let (ws, response) = d
        .websocket(&path, &format!("awman.frontend.v1, awman.ticket.{t}"))
        .await
        .expect("upgrade");
    assert_eq!(response.status().as_u16(), 101);
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "awman.frontend.v1"
    );
    for (_, value) in response.headers() {
        assert!(!value.to_str().unwrap_or("").contains(&t[4..]));
    }
    drop(ws);

    // Replay of the consumed ticket.
    let replay = d
        .websocket(&path, &format!("awman.frontend.v1, awman.ticket.{t}"))
        .await
        .err();
    assert_eq!(replay, Some(401));
    // A ticket for one instance does not open another; it is burnt either way.
    let (_, ticket) = d
        .ticket(&grant, json!({"route":"frontend","instance_id": instance}))
        .await;
    let t2 = ticket["ticket"].as_str().unwrap().to_owned();
    let wrong = d
        .websocket(
            &format!("/v1/engine/instances/{other}/frontend"),
            &format!("awman.frontend.v1, awman.ticket.{t2}"),
        )
        .await
        .err();
    assert_eq!(wrong, Some(401));
    let burnt = d
        .websocket(&path, &format!("awman.frontend.v1, awman.ticket.{t2}"))
        .await
        .err();
    assert_eq!(burnt, Some(401));
    // No ticket, two tickets, or a ticket without a real protocol.
    let (_, ticket) = d
        .ticket(&grant, json!({"route":"frontend","instance_id": instance}))
        .await;
    let t3 = ticket["ticket"].as_str().unwrap().to_owned();
    for offer in [
        "awman.frontend.v1".to_owned(),
        format!("awman.ticket.{t3}"),
        format!("awman.frontend.v1, awman.ticket.{t3}, awman.ticket.{t3}"),
    ] {
        assert_eq!(d.websocket(&path, &offer).await.err(), Some(401), "{offer}");
    }
    // A ticket request for a malformed body is refused.
    let (status, _) = d
        .ticket(
            &grant,
            json!({"route":"frontend","instance_id": instance, "origin": DEV}),
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn expired_tickets_are_refused() {
    let d = daemon().await;
    let grant = d.grant(ORIGIN).await;
    let instance = d.browser_instance(&grant).await;
    // Mint a ticket that is already past its lifetime.
    let auth = d.api.runtime.browser_auth();
    let ticket = auth
        .issue_ticket_for_tests(
            grant["id"].as_str().unwrap(),
            crate::data::browser_auth::TicketRoute::Frontend,
            &instance,
            ORIGIN,
            "loopback",
            chrono::Utc::now() - chrono::Duration::seconds(31),
        )
        .unwrap();
    let refused = d
        .websocket(
            &format!("/v1/engine/instances/{instance}/frontend"),
            &format!("awman.frontend.v1, awman.ticket.{ticket}"),
        )
        .await
        .err();
    assert_eq!(refused, Some(401));
}

#[tokio::test]
async fn deletion_between_ticket_validation_and_watch_refuses_both_upgrades() {
    use crate::data::browser_auth::{GrantRevocationReason, TicketRoute};
    use crate::data::ids::BrowserGrantId;
    use std::sync::atomic::{AtomicBool, Ordering};
    let armed = Arc::new(AtomicBool::new(false));
    let hook_armed = armed.clone();
    let d = daemon_with_hook(
        &["engine"],
        Some(super::super::routes::BrowserAuthenticatedHook(Arc::new(
            move |api, principal| {
                if hook_armed.swap(false, Ordering::SeqCst) {
                    // Authentication has returned, but no watch has been captured.
                    // Revoke on another thread, and wait until deletion is complete.
                    let auth = api.runtime.browser_auth();
                    let id = principal.grant_id.clone();
                    std::thread::scope(|scope| {
                        scope
                            .spawn(move || {
                                auth.grants()
                                    .delete(&id, GrantRevocationReason::Deleted)
                                    .unwrap()
                            })
                            .join()
                            .unwrap()
                    });
                }
            },
        ))),
    )
    .await;
    for route in [TicketRoute::Frontend, TicketRoute::Terminal] {
        let grant = d.grant(ORIGIN).await;
        let instance = d.browser_instance(&grant).await;
        let (resource, path, protocol) = match route {
            TicketRoute::Frontend => (
                instance.clone(),
                format!("/v1/engine/instances/{instance}/frontend"),
                "awman.frontend.v1",
            ),
            TicketRoute::Terminal => {
                let id = crate::data::ids::TerminalId::new().to_string();
                (
                    id.clone(),
                    format!("/v1/terminals/{id}"),
                    "awman.terminal.v1",
                )
            }
        };
        // The terminal need not exist: auth must refuse the deleted grant
        // before resource admission, rather than reaching a 404 handler.
        let ticket = d
            .api
            .runtime
            .browser_auth()
            .issue_ticket_for_tests(
                grant["id"].as_str().unwrap(),
                route,
                &resource,
                ORIGIN,
                "loopback",
                chrono::Utc::now(),
            )
            .unwrap();
        armed.store(true, Ordering::SeqCst);
        let result = d
            .websocket(&path, &format!("{protocol}, awman.ticket.{ticket}"))
            .await;
        assert_eq!(result.err(), Some(401), "revoked upgrade must fail closed");
        let id = BrowserGrantId::parse(grant["id"].as_str().unwrap()).unwrap();
        assert!(d.api.runtime.browser_auth().watch(&id).is_none());
    }
}

#[tokio::test]
async fn revocation_closes_live_websockets_with_1008() {
    let d = daemon().await;
    let auth = d.api.runtime.browser_auth();
    let mut sockets = Vec::new();
    for _ in 0..3 {
        let grant = d.grant(ORIGIN).await;
        let instance = d.browser_instance(&grant).await;
        let (_, ticket) = d
            .ticket(&grant, json!({"route":"frontend","instance_id": instance}))
            .await;
        let (ws, _) = d
            .websocket(
                &format!("/v1/engine/instances/{instance}/frontend"),
                &format!(
                    "awman.frontend.v1, awman.ticket.{}",
                    ticket["ticket"].as_str().unwrap()
                ),
            )
            .await
            .expect("upgrade");
        sockets.push((grant, ws));
    }
    // browsers.delete → "grant revoked".
    let (grant, mut ws) = sockets.remove(0);
    let (status, _) = d
        .owner(
            reqwest::Method::DELETE,
            &format!("/v1/browsers/{}", grant["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(next_close(&mut ws).await, (1008, "grant revoked".into()));
    // Origin removed from allowedOrigins on reload → "origin removed", for
    // every grant of that origin on that listener.
    auth.revoke(
        "loopback",
        BrowserRevocation::OriginRemoved(
            crate::data::config::network::BrowserOrigin::parse(ORIGIN).unwrap(),
        ),
    );
    for (grant, mut ws) in sockets {
        assert_eq!(next_close(&mut ws).await, (1008, "origin removed".into()));
        let (status, _) = d
            .ticket(
                &grant,
                json!({"route":"frontend","instance_id": "ins_00000000000000000000000000000000"}),
            )
            .await;
        assert_eq!(status, 401);
    }
    // Daemon stop → "daemon stopping" (the sink only revokes; the config
    // still allows the origin, so pair once more).
    let fresh = d.grant(ORIGIN).await;
    let instance = d.browser_instance(&fresh).await;
    let (_, ticket) = d
        .ticket(&fresh, json!({"route":"frontend","instance_id": instance}))
        .await;
    let (mut ws, _) = d
        .websocket(
            &format!("/v1/engine/instances/{instance}/frontend"),
            &format!(
                "awman.frontend.v1, awman.ticket.{}",
                ticket["ticket"].as_str().unwrap()
            ),
        )
        .await
        .expect("upgrade");
    auth.revoke_all(crate::data::browser_auth::GrantRevocationReason::DaemonStopping);
    assert_eq!(next_close(&mut ws).await, (1008, "daemon stopping".into()));
    assert!(auth.grants().list(chrono::Utc::now()).is_empty());
}

#[tokio::test]
async fn oversized_bodies_and_browser_hello_frames_are_refused() {
    let d = daemon().await;
    let big = format!("{{\"polling_secret\":\"{}\"}}", "a".repeat(70 * 1024));
    let response = d
        .browser(
            reqwest::Method::POST,
            "/v1/browser-links/blk_00000000000000000000000000000000/poll",
            ORIGIN,
        )
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    // A chunked (length-less) body is bounded while it is read.
    let chunks =
        futures_util::stream::iter((0..80).map(|_| Ok::<_, std::io::Error>(vec![b' '; 1024])));
    let grant = d.grant(ORIGIN).await;
    let response = d
        .browser(reqwest::Method::POST, "/v1/browser-tickets", ORIGIN)
        .bearer_auth(grant["token"].as_str().unwrap())
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);

    let instance = d.browser_instance(&grant).await;
    let (_, ticket) = d
        .ticket(&grant, json!({"route":"frontend","instance_id": instance}))
        .await;
    let (mut ws, _) = d
        .websocket(
            &format!("/v1/engine/instances/{instance}/frontend"),
            &format!(
                "awman.frontend.v1, awman.ticket.{}",
                ticket["ticket"].as_str().unwrap()
            ),
        )
        .await
        .expect("upgrade");
    use futures_util::SinkExt;
    ws.send(WsMessage::Text("x".repeat(17 * 1024).into()))
        .await
        .unwrap();
    assert_eq!(next_close(&mut ws).await.0, 1009);
}

#[tokio::test]
async fn browser_health_advertises_pairing_capabilities_and_listener_availability() {
    let d = daemon().await;
    let response = d
        .browser(reqwest::Method::GET, "/v1/health", ORIGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let health: Value = response.json().await.unwrap();
    assert_eq!(health["browser_links"], true);
    assert_eq!(health["protocols"]["browser"], 1);
    let capabilities: Vec<&str> = health["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(capabilities.contains(&"browser_links"));
    assert!(capabilities.contains(&"browser_tickets"));
    // No session data in health.
    assert!(health.get("instances").is_none());
    assert!(health.to_string().find(DEV).is_none());
    // Non-browser health is unchanged.
    let plain: Value = d
        .http
        .get(d.url("/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(plain, json!({"status":"ok"}));
    // Owner-socket data is never cacheable either.
    let response = d
        .http
        .get(format!("http://{}/v1/status", d.owner))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn pairing_routes_are_not_served_on_the_owner_socket() {
    let d = daemon().await;
    for path in ["/v1/browser-links", "/v1/browser-tickets"] {
        let (status, _) = d.owner(reqwest::Method::POST, path, Some(json!({}))).await;
        assert_eq!(status, 404, "{path}");
    }
    // And owner routes are not served on the browser listener, even with a grant.
    let grant = d.grant(ORIGIN).await;
    let (status, _, _) = d
        .with_grant(
            reqwest::Method::GET,
            "/v1/browser-links?code=web-AAAA-AAAA",
            &grant,
            ORIGIN,
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn browsers_never_see_worker_terminals() {
    use crate::data::daemon_state::FeatureName;
    use crate::data::terminal::{TerminalOwner, TerminalOwnerInfo};
    use crate::engine::terminal_hub::{TerminalInput, TerminalRegistration};
    let d = daemon_with(&["engine", "worker"]).await;
    let grant = d.grant(ORIGIN).await;
    let (stdin, _rx) = crate::engine::terminal_hub::input_channel();
    let handle = d.api.terminals.hub().register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(crate::data::ids::CommandId::new()),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: true,
            },
            agent: "claude".into(),
            model: None,
            cols: 40,
            rows: 10,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    let id = handle.id().to_string();
    let (status, owner) = d.owner(reqwest::Method::GET, "/v1/terminals", None).await;
    assert_eq!(status, 200);
    assert_eq!(owner["items"].as_array().unwrap().len(), 1);
    let (status, _, list) = d
        .with_grant(reqwest::Method::GET, "/v1/terminals", &grant, ORIGIN)
        .await;
    assert_eq!(status, 200, "{list}");
    assert!(list["items"].as_array().unwrap().is_empty());
    let (status, _, refused) = d
        .with_grant(
            reqwest::Method::GET,
            &format!("/v1/terminals/{id}"),
            &grant,
            ORIGIN,
        )
        .await;
    assert_eq!(
        (status, refused["code"].as_str()),
        (404, Some("terminal_not_found"))
    );
    let (status, _) = d
        .ticket(&grant, json!({"route":"terminal","terminal_id": id}))
        .await;
    assert_eq!(status, 404);
    // Without a ticket a browser cannot upgrade a terminal socket at all.
    let refused = d
        .websocket(&format!("/v1/terminals/{id}"), "awman.terminal.v1")
        .await
        .err();
    assert_eq!(refused, Some(401));
}

#[tokio::test]
async fn browser_launch_context_authority_catalogue_and_separate_credentials() {
    let d = daemon().await;
    let repo = tempfile::tempdir().unwrap();
    let (status, context) = d
        .owner(
            reqwest::Method::POST,
            "/v1/engine/launch_contexts",
            Some(json!({"root":repo.path(),"env":{"PRIVATE":"planted-secret"},"default":true})),
        )
        .await;
    assert_eq!(status, 200, "{context}");
    let grant = d.grant(ORIGIN).await;
    let (status, _, list) = d
        .with_grant(
            reqwest::Method::GET,
            "/v1/engine/launch_contexts",
            &grant,
            ORIGIN,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(list["default_id"], context["id"]);
    assert!(!list.to_string().contains("planted-secret"));
    let (status, _, _) = d
        .with_grant(
            reqwest::Method::POST,
            "/v1/engine/launch_contexts",
            &grant,
            ORIGIN,
        )
        .await;
    assert_eq!(status, 404);
    let root = d.api.runtime.daemon_paths().root().parent().unwrap();
    assert!(!root.join("work_servers").exists());
    let response = d
        .browser(reqwest::Method::GET, "/v1/engine/launch_contexts", ORIGIN)
        .bearer_auth("server-refresh-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let response = d
        .browser(reqwest::Method::GET, "/v1/engine/catalogue", ORIGIN)
        .bearer_auth(grant["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let catalogue: Value = response.json().await.unwrap();
    assert!(!catalogue["commands"].as_array().unwrap().is_empty());
    let response = d
        .browser(reqwest::Method::GET, "/v1/engine/catalogue", ORIGIN)
        .bearer_auth(grant["token"].as_str().unwrap())
        .header("If-None-Match", etag)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 304);
    let (status, _, _) = d
        .with_grant(
            reqwest::Method::GET,
            "/v1/engine/prompts?kind=no_such_prompt",
            &grant,
            ORIGIN,
        )
        .await;
    assert_eq!(status, 404);
    for action in ["pause", "resume", "trigger", "cancel", "delete"] {
        let (status, _, p) = d
            .with_grant(
                reqwest::Method::GET,
                &format!("/v1/engine/prompts?kind=squad_task_confirm&action={action}&name=demo"),
                &grant,
                ORIGIN,
            )
            .await;
        assert_eq!(status, 200, "{p}");
        assert!(p["body"].as_str().unwrap().contains("demo"));
    }
}

#[tokio::test]
async fn browser_hello_cannot_impersonate_native_or_supply_host_inputs() {
    use futures_util::SinkExt;
    let d = daemon().await;
    let grant = d.grant(ORIGIN).await;
    for fields in [
        json!({"kind":"tui","cwd":"/tmp","env":{},"argv":[]}),
        json!({"kind":"web","env":{"SECRET":"planted"},"cwd":"","argv":[],"launch_context":crate::data::ids::LaunchContextId::new()}),
        json!({"kind":"web","cwd":"","env":{},"argv":[],"executable":"/bin/sh"}),
    ] {
        let id = d.browser_instance(&grant).await;
        let (_, ticket) = d
            .ticket(&grant, json!({"route":"frontend","instance_id":id}))
            .await;
        let (mut ws, _) = d
            .websocket(
                &format!("/v1/engine/instances/{id}/frontend"),
                &format!(
                    "awman.frontend.v1, awman.ticket.{}",
                    ticket["ticket"].as_str().unwrap()
                ),
            )
            .await
            .unwrap();
        let mut hello = json!({"protocol":"awman.frontend.v1","build":"test","role":"primary","annotations":{},"term_size":{"cols":80,"rows":24},"stdin_is_tty":false,"stdout_is_tty":false,"view":null,"resume":null});
        hello
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        hello["seq"] = json!(1);
        hello["type"] = json!("hello");
        ws.send(WsMessage::Text(hello.to_string().into()))
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WsMessage::Text(text) = frame else {
            panic!("expected protocol refusal")
        };
        let error: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(error["code"], "protocol_error");
        assert!(!text.contains("planted"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn granted_web_hello_resolves_owner_context_and_resumes() {
    use crate::data::frontend_protocol::{Codec, EngineFrame, FrontendFrame};
    use futures_util::SinkExt;
    let d = daemon().await;
    let repo = tempfile::tempdir().unwrap();
    let (_, context) = d.owner(reqwest::Method::POST, "/v1/engine/launch_contexts", Some(json!({
        "root": repo.path(), "default": true,
        "env": { "HOME": d._tmp.path(), "AWMAN_HOME": d._tmp.path(), "PATH": "/usr/bin:/bin", "AWMAN_TEST_ISOLATION": "1", "PRIVATE": "context-secret" }
    }))).await;
    let grant = d.grant(ORIGIN).await;
    let instance = d.browser_instance(&grant).await;
    let path = format!("/v1/engine/instances/{instance}/frontend");
    let mut frontend_id = None;
    for reconnect in [false, true] {
        let (_, ticket) = d
            .ticket(&grant, json!({"route":"frontend", "instance_id":instance}))
            .await;
        let (mut ws, _) = d
            .websocket(
                &path,
                &format!(
                    "awman.frontend.v1, awman.ticket.{}",
                    ticket["ticket"].as_str().unwrap()
                ),
            )
            .await
            .unwrap();
        let mut hello = json!({
            "protocol":"awman.frontend.v1", "build":"test", "kind":"web", "role":"primary",
            "annotations":{}, "term_size":{"cols":80,"rows":24}, "view":null,
            "resume":frontend_id,
        });
        if !reconnect {
            hello["launch_context"] = context["id"].clone();
        }
        let hello: crate::data::frontend_protocol::Hello = serde_json::from_value(hello).unwrap();
        ws.send(WsMessage::Text(
            Codec::new()
                .encode(&FrontendFrame::Hello(hello))
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
        let mut decoder = Codec::new();
        for expected in ["welcome", "snapshot"] {
            let text = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let WsMessage::Text(text) = text else {
                panic!("expected {expected}")
            };
            assert!(!text.contains("context-secret"));
            let (_, frame) = decoder
                .decode::<EngineFrame>(&text)
                .unwrap_or_else(|e| panic!("expected {expected}: {e}; {text}"));
            match frame {
                EngineFrame::Welcome {
                    frontend_id: id, ..
                } => {
                    if reconnect {
                        assert_eq!(frontend_id.as_ref(), Some(&id));
                    }
                    frontend_id = Some(id);
                }
                EngineFrame::Snapshot(snapshot) => assert_eq!(
                    snapshot.tabs[0].session.cwd,
                    repo.path().canonicalize().unwrap()
                ),
                _ => panic!("unexpected {expected} frame"),
            }
        }
        ws.close(None).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
