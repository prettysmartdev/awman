use super::support::Api;
use crate::support;
use awman::command::daemon_api::{browser, keys};
use serde_json::json;

const ORIGIN: &str = "https://awman.prettysmart.dev";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_an_origin_ends_grant_watchers_and_pending_links_before_reload_returns() {
    use awman::data::{browser_auth::GrantRevocationReason, ids::BrowserGrantId};
    let sandbox = support::sandbox("daemon::browser::removing_an_origin_ends_grant_watchers_and_pending_links_before_reload_returns");
    let server = Api::start(&sandbox).await;
    let ctx = browser::BrowserContext {
        origin: ORIGIN.into(),
        listener: "component".into(),
    };
    let created = browser::links_create(&server.api, &ctx).unwrap();
    sandbox.redact(created.polling_secret.expose());
    browser::links_decide(&server.api, created.id.as_str(), true).unwrap();
    let awman::data::browser_auth::BrowserLinkPollResponse::Approved { grant } =
        browser::links_poll(
            &server.api,
            &ctx,
            created.id.as_str(),
            created.polling_secret.expose(),
        )
        .unwrap()
    else {
        panic!("approved grant missing")
    };
    sandbox.redact(grant.token.expose());
    let id = BrowserGrantId::parse(grant.id.as_str()).unwrap();
    let watch = server.api.runtime.browser_auth().watch(&id).unwrap();
    let pending = browser::links_create(&server.api, &ctx).unwrap();
    sandbox.redact(pending.polling_secret.expose());
    let path =
        awman::data::config::file::global_config_path(&support::env_snapshot(&sandbox)).unwrap();
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["daemon"]["network"]["listeners"]["component"]["allowedOrigins"] = json!([]);
    std::fs::write(path, config.to_string()).unwrap();
    server.api.runtime.reload_config().await.unwrap();
    assert!(watch.is_revoked());
    assert_eq!(watch.ended().await, GrantRevocationReason::OriginRemoved);
    assert!(browser::browsers_list(&server.api).items.is_empty());
    assert!(matches!(
        browser::links_poll(
            &server.api,
            &ctx,
            pending.id.as_str(),
            pending.polling_secret.expose()
        ),
        Err(browser::BrowserError::LinkGone)
    ));
    assert_eq!(
        server
            .request(
                "GET",
                "/v1/status",
                Some(grant.token.expose()),
                Some(ORIGIN),
                None
            )
            .await
            .0,
        403
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_pairing_is_one_use_origin_bound_and_owner_revocable() {
    let sandbox = support::sandbox(
        "daemon::browser::browser_pairing_is_one_use_origin_bound_and_owner_revocable",
    );
    let server = Api::start(&sandbox).await;
    let (code, created) = server
        .request(
            "POST",
            "/v1/browser-links",
            None,
            Some(ORIGIN),
            Some(json!({})),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    sandbox.redact(created["polling_secret"].as_str().unwrap());
    let view = browser::links_lookup(&server.api, created["code"].as_str().unwrap()).unwrap();
    assert_eq!(view.listener, "component");
    let decision =
        browser::links_decide(&server.api, created["id"].as_str().unwrap(), true).unwrap();
    let path = format!("/v1/browser-links/{}/poll", created["id"].as_str().unwrap());
    let (code, polled) = server
        .request(
            "POST",
            &path,
            None,
            Some(ORIGIN),
            Some(json!({"polling_secret":created["polling_secret"]})),
        )
        .await;
    assert_eq!(code, 200, "{polled}");
    assert_eq!(polled["status"], "approved");
    let token = polled["grant"]["token"].as_str().unwrap();
    sandbox.redact(token);
    let (code, status) = server
        .request("GET", "/v1/status", Some(token), Some(ORIGIN), None)
        .await;
    assert_eq!(code, 200, "{status}");
    for path in [
        "/v1/worker/commands",
        "/v1/keys",
        "/v1/config",
        "/v1/browsers",
    ] {
        assert_eq!(
            server
                .request("GET", path, Some(token), Some(ORIGIN), None)
                .await
                .0,
            404,
            "{path}"
        );
    }
    assert_eq!(
        server
            .request(
                "GET",
                "/v1/status",
                Some(token),
                Some("https://evil.invalid"),
                None
            )
            .await
            .0,
        403
    );
    assert_eq!(server.get("/v1/status", Some(token)).await.0, 401);
    let metadata = serde_json::to_string(&browser::browsers_list(&server.api)).unwrap();
    assert!(!metadata.contains(token));
    let disk =
        std::fs::read_to_string(server.api.runtime.daemon_paths().browser_grants_file()).unwrap();
    assert!(!disk.contains(token));
    assert_eq!(
        server
            .request(
                "POST",
                &path,
                None,
                Some(ORIGIN),
                Some(json!({"polling_secret":created["polling_secret"]}))
            )
            .await
            .0,
        410
    );
    browser::browsers_delete(&server.api, decision.grant_id.unwrap().as_str()).unwrap();
    assert_eq!(
        server
            .request("GET", "/v1/status", Some(token), Some(ORIGIN), None)
            .await
            .0,
        401
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_origin_never_authorizes_api_keys_or_caller_selected_scope() {
    let sandbox = support::sandbox(
        "daemon::browser::browser_origin_never_authorizes_api_keys_or_caller_selected_scope",
    );
    let server = Api::start(&sandbox).await;
    let key = keys::keys_create(&server.api, "non-browser").unwrap();
    sandbox.redact(&key.key);
    assert_eq!(
        server
            .request("GET", "/v1/status", Some(&key.key), Some(ORIGIN), None)
            .await
            .0,
        401
    );
    for body in [
        json!({"scope":"worker"}),
        json!({"origin":ORIGIN}),
        json!({"listener":"component"}),
        json!([]),
    ] {
        let (code, response) = server
            .request("POST", "/v1/browser-links", None, Some(ORIGIN), Some(body))
            .await;
        assert_eq!(code, 400, "{response}");
        assert_eq!(response["code"], "invalid_request");
    }
    assert_eq!(
        server
            .request(
                "POST",
                "/v1/browser-links",
                None,
                Some("null"),
                Some(json!({}))
            )
            .await
            .0,
        403
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/v1/browser-links",
                None,
                Some("https://evil.invalid"),
                Some(json!({}))
            )
            .await
            .0,
        403
    );
    server.shutdown().await;
    sandbox.finish();
}
