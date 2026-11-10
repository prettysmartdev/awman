use awman_wsp::*;
use serde_json::json;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test]
async fn pending_link_response_can_slow_polling_interval() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/link/link_1/token"))
        .and(header(VERSION_HEADER, WSP_VERSION))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"interval_ms":80})))
        .expect(2)
        .mount(&server)
        .await;
    let client = Client::new(&server.uri(), String::new()).unwrap();
    assert!(client.poll_link("link_1").await.unwrap().is_none());
    let start = tokio::time::Instant::now();
    assert!(client.poll_link("link_1").await.unwrap().is_none());
    assert!(start.elapsed() >= std::time::Duration::from_millis(70));
}
#[tokio::test]
async fn claim_requires_created_and_preserves_typed_conflicts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/claims"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"lease":{}})))
        .mount(&server)
        .await;
    let client = Client::new(&server.uri(), "refresh".into()).unwrap();
    assert!(matches!(
        client.claim(None, 0).await,
        Err(Error::InvalidResponse)
    ));
    for code in [codes::TAKEN, codes::LEASE_LOST, codes::INELIGIBLE] {
        let error = Error::Conflict {
            code: code.into(),
            message: "conflict".into(),
        };
        assert_eq!(error.is_taken(), code == codes::TAKEN);
        assert_eq!(error.is_lease_lost(), code == codes::LEASE_LOST);
        assert_eq!(error.is_ineligible(), code == codes::INELIGIBLE);
    }
}

#[tokio::test]
async fn unsupported_claim_requirements_are_released_with_the_original_epoch() {
    for pointer in [
        "/item/exec/network",
        "/item/exec/readOnly",
        "/item/repo/credentials",
        "/item/exec/interaction",
        "/item/outputs/mount_mode",
        "/item/affinity/required_extra",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})),
            )
            .mount(&server)
            .await;
        let mut lease = json!({"id":"lease-reused", "epoch":42,
            "expires_at":"2099-01-01T00:00:00Z", "item":conformance::work_item("item")});
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        if parent == "/item/outputs" {
            lease["item"]["outputs"] = json!({"max_bytes":100});
        }
        let value = match pointer {
            "/item/repo/credentials" => json!({"kind":"ssh_agent","token":"NEVER_ECHO"}),
            "/item/exec/interaction" => json!("unsupported"),
            _ => json!(true),
        };
        lease
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.into(), value);
        Mock::given(method("POST"))
            .and(path("/v1/claims"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"lease":lease})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/v1/leases/lease-reused/release"))
            .and(header("x-wsp-epoch", "42"))
            .and(wiremock::matchers::body_json(json!({"reason":"ineligible", "message":"unsupported or invalid lease requirements"})))
            .respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
        let client = Client::new(&server.uri(), "refresh".into()).unwrap();
        let error = client.claim(None, 0).await.unwrap_err();
        assert!(error.is_ineligible(), "{pointer}: {error}");
        assert!(!error.to_string().contains("NEVER_ECHO"));
    }
}

#[tokio::test]
async fn terminal_handshake_preserves_lease_lost_conflict() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"access"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/leases/old/terminal"))
        .and(header("x-wsp-epoch", "7"))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(json!({"error":"lease lost","code":"lease_lost"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(&server.uri(), "refresh".into())
        .unwrap()
        .with_lease_epoch("old", 7);
    let error = match client.open_terminal("old").await {
        Ok(_) => panic!("stale lease admitted"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Conflict { ref code, .. } if code == "lease_lost"));
}
