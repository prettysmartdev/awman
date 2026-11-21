//! HTTP-only client decisions use owned listeners and exact request counts.
use crate::support;
use awman_wsp::{AccessToken, Client, Error, RegisterRequest, VERSION_HEADER, WSP_VERSION};
use chrono::{Duration, Utc};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_requests_preserve_base_path_version_and_query() {
    let sandbox = support::sandbox(
        "worker::client_transport::authenticated_requests_preserve_base_path_version_and_query",
    );
    let server = MockServer::builder()
        .listener(sandbox.ports().tcp_listener())
        .start()
        .await;
    Mock::given(method("POST"))
        .and(path("/base/v1/token"))
        .and(header(VERSION_HEADER, WSP_VERSION))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/base/v1/workers/register"))
        .and(header(VERSION_HEADER, WSP_VERSION))
        .and(header("authorization", "Bearer access"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"worker_id":"worker_1","server_labels":{"owner":"alice"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/base/v1/queue"))
        .and(header(VERSION_HEADER, WSP_VERSION))
        .and(header("authorization", "Bearer access"))
        .and(query_param("limit", "50"))
        .and(query_param("cursor", "opaque+/="))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"items":[],"next_cursor":null})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/base/v1/claims"))
        .and(query_param("wait_ms", "0"))
        .and(header(VERSION_HEADER, WSP_VERSION))
        .and(header("authorization", "Bearer access"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(&format!("{}/base", server.uri()), "refresh".into()).unwrap();
    let registered = client
        .register(&RegisterRequest {
            name: "worker".into(),
            labels: Default::default(),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap();
    assert_eq!(registered.server_labels["owner"], "alice");
    assert!(client
        .queue(50, Some("opaque+/="))
        .await
        .unwrap()
        .items
        .is_empty());
    assert!(client.claim(None, 0).await.unwrap().is_none());
    let requests = server.received_requests().await.unwrap();
    let claim = requests
        .iter()
        .find(|r| r.url.path().ends_with("claims"))
        .unwrap();
    assert_eq!(
        claim
            .url
            .query_pairs()
            .find(|(name, _)| name == "wait_ms")
            .unwrap()
            .1,
        "0"
    );
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_refresh_is_single_flight_and_revoked_or_twice_rejected_tokens_stop() {
    let sandbox = support::sandbox("worker::client_transport::access_refresh_is_single_flight_and_revoked_or_twice_rejected_tokens_stop");
    for mode in ["expired", "twice-rejected", "revoked"] {
        let server = MockServer::builder()
            .listener(sandbox.ports().tcp_listener())
            .start()
            .await;
        let refresh = if mode == "revoked" {
            ResponseTemplate::new(403)
                .set_body_json(json!({"error":"revoked","code":"access_denied"}))
        } else {
            ResponseTemplate::new(200).set_body_json(json!({"access_token":"fresh"}))
        };
        Mock::given(method("POST"))
            .and(path("/v1/token"))
            .and(header(VERSION_HEADER, WSP_VERSION))
            .respond_with(refresh)
            .expect(1)
            .mount(&server)
            .await;
        if mode != "expired" {
            Mock::given(method("GET"))
                .and(path("/v1/queue"))
                .and(header("authorization", "Bearer stale"))
                .respond_with(
                    ResponseTemplate::new(401)
                        .set_body_json(json!({"error":"refused","code":"unauthorized"})),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        if mode != "revoked" {
            let response = if mode == "expired" {
                ResponseTemplate::new(200).set_body_json(json!({"items":[],"next_cursor":null}))
            } else {
                ResponseTemplate::new(401)
                    .set_body_json(json!({"error":"refused","code":"unauthorized"}))
            };
            Mock::given(method("GET"))
                .and(path("/v1/queue"))
                .and(header("authorization", "Bearer fresh"))
                .respond_with(response)
                .expect(if mode == "expired" { 8 } else { 1 })
                .mount(&server)
                .await;
        }
        let client = Client::new(&server.uri(), "refresh-secret".into())
            .unwrap()
            .with_access_token_cache(AccessToken {
                access_token: "stale".into(),
                expires_at: Some(
                    Utc::now()
                        + if mode == "expired" {
                            -Duration::hours(1)
                        } else {
                            Duration::hours(1)
                        },
                ),
            })
            .await;
        if mode == "expired" {
            let mut requests = tokio::task::JoinSet::new();
            for _ in 0..8 {
                let client = client.clone();
                requests.spawn(async move { client.queue(50, None).await });
            }
            while let Some(result) = requests.join_next().await {
                assert!(result.unwrap().unwrap().items.is_empty());
            }
        } else if mode == "revoked" {
            assert!(matches!(
                client.queue(50, None).await,
                Err(Error::LinkRevoked)
            ));
        } else {
            assert_eq!(
                client.queue(50, None).await.unwrap_err().code(),
                Some("unauthorized")
            );
        }
        drop(server);
    }
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_denial_and_unsafe_identifiers_never_become_transport_requests() {
    let sandbox = support::sandbox("worker::client_transport::link_denial_and_unsafe_identifiers_never_become_transport_requests");
    let server = MockServer::builder()
        .listener(sandbox.ports().tcp_listener())
        .start()
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/link/link_1/token"))
        .and(header(VERSION_HEADER, WSP_VERSION))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"error":"denied","code":"access_denied"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(&server.uri(), String::new()).unwrap();
    assert!(matches!(
        client.poll_link("link_1").await,
        Err(Error::LinkDenied)
    ));
    for id in ["../bad", "", "a/b", "a?b", "a#b"] {
        assert!(
            matches!(client.poll_link(id).await, Err(Error::InvalidUrl)),
            "{id}"
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    drop(server);
    sandbox.finish();
}
