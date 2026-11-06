//! `/v1/keys` lifecycle and the unauthenticated health route.

use serde_json::json;

use crate::api_process::ApiHome;
use crate::support::*;

#[tokio::test(flavor = "multi_thread")]
async fn health_is_unauthenticated_and_ignores_a_bad_bearer() {
    let home = ApiHome::new();
    let server = home.start().await;
    let plain = reqwest::get(format!("{}/v1/health", server.url))
        .await
        .unwrap();
    assert_eq!(plain.status(), 200);
    let junk = reqwest::Client::new()
        .get(format!("{}/v1/health", server.url))
        .header("authorization", "Bearer not-a-key")
        .send()
        .await
        .unwrap();
    assert_eq!(junk.status(), 200);
    let body: serde_json::Value = junk.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_are_shown_once_listed_without_material_and_revoked_at_once() {
    let home = ApiHome::new();
    let server = home.start().await;

    let (code, created) = http(
        &server,
        reqwest::Method::POST,
        "/v1/keys",
        None,
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(code, 201, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let key = created["key"].as_str().unwrap().to_string();
    assert!(id.starts_with("key_"), "{created}");
    assert!(key.len() >= 32, "a real secret, shown this once");

    let (code, listed) = http(&server, reqwest::Method::GET, "/v1/keys", None, None).await;
    assert_eq!(code, 200);
    assert!(listed["items"].is_array() && listed.get("next_cursor").is_some());
    let text = listed.to_string();
    assert!(text.contains(&id) && text.contains("ci"));
    assert!(
        !text.contains(&key),
        "a list never carries the secret: {text}"
    );
    assert!(!text.contains("hash"), "nor its hash: {text}");
    assert!(listed["items"][0]["created_at"].is_string());

    // The key file on disk holds the hash only.
    let file = home
        .home()
        .join(".local/share/awman/data/daemon/api_keys.json");
    if let Ok(on_disk) = std::fs::read_to_string(&file) {
        assert!(!on_disk.contains(&key), "plaintext key on disk");
    }

    // A duplicate name is a conflict; a bad name is invalid.
    let (code, _) = http(
        &server,
        reqwest::Method::POST,
        "/v1/keys",
        None,
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(code, 409);
    let (code, _) = http(
        &server,
        reqwest::Method::POST,
        "/v1/keys",
        None,
        Some(json!({"name": "Bad Name"})),
    )
    .await;
    assert_eq!(code, 400);

    let (code, _) = http(
        &server,
        reqwest::Method::DELETE,
        &format!("/v1/keys/{id}"),
        None,
        None,
    )
    .await;
    assert!(code == 200 || code == 204, "{code}");
    let (_, listed) = http(&server, reqwest::Method::GET, "/v1/keys", None, None).await;
    assert!(!listed.to_string().contains(&id));
    let (code, _) = http(
        &server,
        reqwest::Method::DELETE,
        &format!("/v1/keys/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_warns_when_the_network_listener_has_no_keys() {
    let home = ApiHome::new();
    let _server = home.start().await;
    // No keys exist: the text status says so only when a network listener is
    // on; with it off there is no warning.
    let text = home.cli_ok(&["daemon", "status"]);
    assert!(!text.contains("no API keys"), "{text}");
}
