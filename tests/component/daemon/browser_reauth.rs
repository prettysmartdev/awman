//! WI 0160 reload-safe browser reauthorization through the production
//! router: enrollment, proof, expiry, revocation, daemon restart, origin and
//! listener binding, replay, wrong keys and two tabs. The signer and the
//! transcript here are written independently of the daemon's own code from
//! the work item's C3 byte definition.
use super::support::{merge, Api};
use crate::support;
use awman::command::daemon_api::browser::{self, BrowserContext, BrowserError};
use awman::data::browser_auth::{
    BrowserReauthChallengeRequest, BrowserReauthCompleteRequest, BrowserSecret,
    GrantRevocationReason,
};
use awman::data::ids::BrowserGrantId;
use base64::Engine as _;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const ORIGIN: &str = "https://awman.prettysmart.dev";
const OTHER_ORIGIN: &str = "http://127.0.0.1:3000";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_id(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    SystemRandom::new().fill(&mut bytes).unwrap();
    b64(&bytes)
}

/// One browser profile: a P-256 key and a fresh credential ID.
struct Browser {
    pair: EcdsaKeyPair,
    credential_id: String,
}

impl Browser {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        Self {
            pair: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                .unwrap(),
            credential_id: random_id(32),
        }
    }

    fn enrollment(&self) -> Value {
        json!({"reauth":{"version":1,"credential_id":self.credential_id,
            "algorithm":"ECDSA_P256_SHA256","public_key":b64(self.pair.public_key().as_ref())}})
    }

    /// C3's transcript, built from the challenge response's exact strings.
    fn sign(&self, challenge: &Value) -> String {
        let mut message = b"awman.browser.reauth.v1\0".to_vec();
        let fields = [
            "credential_id",
            "daemon_epoch",
            "listener_epoch",
            "origin",
            "listener",
            "challenge_id",
            "nonce",
            "client_nonce",
            "expires_at",
        ];
        let values = ["POST", "/v1/browser-reauth/complete"]
            .into_iter()
            .map(str::to_owned)
            .chain(
                fields
                    .iter()
                    .map(|f| challenge[*f].as_str().unwrap().to_owned()),
            );
        for value in values {
            message.extend_from_slice(&(value.len() as u32).to_be_bytes());
            message.extend_from_slice(value.as_bytes());
        }
        b64(self
            .pair
            .sign(&SystemRandom::new(), &message)
            .unwrap()
            .as_ref())
    }
}

/// The public epochs a browser saves from health.
#[derive(Clone, Debug)]
struct Epochs {
    daemon: String,
    listener: String,
}

async fn epochs(server: &Api, origin: &str) -> Epochs {
    let (code, health) = server
        .request("GET", "/v1/health", None, Some(origin), None)
        .await;
    assert_eq!(code, 200, "{health}");
    assert_eq!(health["protocols"]["browser_reauth"], 1, "{health}");
    assert!(health["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("browser_reauth")));
    let reauth = &health["browser_reauth"];
    assert_eq!(reauth["version"], 1);
    Epochs {
        daemon: reauth["daemon_epoch"].as_str().unwrap().to_owned(),
        listener: reauth["listener_epoch"].as_str().unwrap().to_owned(),
    }
}

async fn challenge(
    server: &Api,
    origin: &str,
    credential_id: &str,
    epochs: &Epochs,
) -> (u16, Value) {
    server
        .request(
            "POST",
            "/v1/browser-reauth/challenge",
            None,
            Some(origin),
            Some(json!({"version":1,"credential_id":credential_id,
                "daemon_epoch":epochs.daemon,"listener_epoch":epochs.listener,
                "client_nonce":random_id(32)})),
        )
        .await
}

async fn complete(
    server: &Api,
    origin: &str,
    challenge_id: &Value,
    signature: &str,
) -> (u16, Value) {
    server
        .request(
            "POST",
            "/v1/browser-reauth/complete",
            None,
            Some(origin),
            Some(json!({"version":1,"challenge_id":challenge_id,"signature":signature})),
        )
        .await
}

/// Challenge then sign then complete, as a browser tab would.
async fn prove(
    server: &Api,
    sandbox: &awman_test_support::TestSandbox,
    origin: &str,
    browser: &Browser,
    epochs: &Epochs,
) -> (u16, Value) {
    let (code, issued) = challenge(server, origin, &browser.credential_id, epochs).await;
    assert_eq!(code, 201, "{issued}");
    let (code, body) = complete(
        server,
        origin,
        &issued["challenge_id"],
        &browser.sign(&issued),
    )
    .await;
    if let Some(token) = body["grant"]["token"].as_str() {
        sandbox.redact(token);
    }
    (code, body)
}

/// Create a key-bound link and approve it (owner); returns the create body.
async fn link_and_approve(
    server: &Api,
    sandbox: &awman_test_support::TestSandbox,
    origin: &str,
    browser: &Browser,
) -> Value {
    let (code, created) = server
        .request(
            "POST",
            "/v1/browser-links",
            None,
            Some(origin),
            Some(browser.enrollment()),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    sandbox.redact(created["polling_secret"].as_str().unwrap());
    browser::links_decide(&server.api, created["id"].as_str().unwrap(), true).unwrap();
    created
}

async fn poll(server: &Api, origin: &str, created: &Value) -> (u16, Value) {
    server
        .request(
            "POST",
            &format!("/v1/browser-links/{}/poll", created["id"].as_str().unwrap()),
            None,
            Some(origin),
            Some(json!({"polling_secret":created["polling_secret"]})),
        )
        .await
}

fn ctx(origin: &str, listener: &str) -> BrowserContext {
    BrowserContext {
        origin: origin.into(),
        listener: listener.into(),
    }
}

/// Policy-level challenge for clock-controlled tests.
fn challenge_at(
    server: &Api,
    ctx: &BrowserContext,
    browser: &Browser,
    epochs: &Epochs,
    now: chrono::DateTime<chrono::Utc>,
    instant: Instant,
) -> Value {
    let issued = browser::reauth_challenge_at(
        &server.api,
        ctx,
        &BrowserReauthChallengeRequest {
            version: 1,
            credential_id: browser.credential_id.clone(),
            daemon_epoch: epochs.daemon.clone(),
            listener_epoch: epochs.listener.clone(),
            client_nonce: random_id(32),
        },
        now,
        instant,
    )
    .unwrap();
    serde_json::to_value(issued).unwrap()
}

fn complete_at(
    server: &Api,
    ctx: &BrowserContext,
    challenge: &Value,
    signature: String,
    now: chrono::DateTime<chrono::Utc>,
    instant: Instant,
) -> Result<awman::data::browser_auth::BrowserReauthCompleteResponse, BrowserError> {
    browser::reauth_complete_at(
        &server.api,
        ctx,
        &BrowserReauthCompleteRequest {
            version: 1,
            challenge_id: challenge["challenge_id"].as_str().unwrap().to_owned(),
            signature: BrowserSecret::new(signature),
        },
        now,
        instant,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_proof_recovers_the_same_grant_bearer_and_expiry_without_a_new_link() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::valid_proof_recovers_the_same_grant_bearer_and_expiry_without_a_new_link",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let (code, created) = server
        .request(
            "POST",
            "/v1/browser-links",
            None,
            Some(ORIGIN),
            Some(tab.enrollment()),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    sandbox.redact(created["polling_secret"].as_str().unwrap());
    assert_eq!(
        created["reauth"],
        json!({"version":1,"credential_id":tab.credential_id,
            "daemon_epoch":saved.daemon,"listener_epoch":saved.listener})
    );
    // Not yet approved: generic failure.
    assert_eq!(prove(&server, &sandbox, ORIGIN, &tab, &saved).await.0, 401);
    // The owner sees the key's fingerprint before approving.
    let view = browser::links_lookup(&server.api, created["code"].as_str().unwrap()).unwrap();
    let key = view.reauth.expect("key-bound link shows its key");
    assert_eq!(key.credential_id, tab.credential_id);
    assert_eq!(key.key_sha256.len(), 43);
    browser::links_decide(&server.api, created["id"].as_str().unwrap(), true).unwrap();
    let (code, polled) = poll(&server, ORIGIN, &created).await;
    assert_eq!(code, 200, "{polled}");
    let grant = polled["grant"].clone();
    sandbox.redact(grant["token"].as_str().unwrap());
    assert_eq!(grant["reauth"], created["reauth"]);
    // Reload: the bearer is gone from memory; proof returns the same one.
    for _ in 0..2 {
        let (code, recovered) = prove(&server, &sandbox, ORIGIN, &tab, &saved).await;
        assert_eq!(code, 200, "{recovered}");
        assert_eq!(
            recovered["grant"], grant,
            "same id, token, expiry and binding"
        );
    }
    let token = grant["token"].as_str().unwrap();
    assert_eq!(
        server
            .request("GET", "/v1/status", Some(token), Some(ORIGIN), None)
            .await
            .0,
        200
    );
    // Polling stays one-use; the owner list shows the grant is recoverable.
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 410);
    let listed = browser::browsers_list(&server.api);
    assert_eq!(
        listed.items[0].reauth.as_ref().unwrap().credential_id,
        tab.credential_id
    );
    // A credential ID cannot be enrolled twice.
    let (code, refused) = server
        .request(
            "POST",
            "/v1/browser-links",
            None,
            Some(ORIGIN),
            Some(tab.enrollment()),
        )
        .await;
    assert_eq!(
        (code, refused["code"].as_str()),
        (409, Some("credential_exists"))
    );
    // Nothing secret reaches the grant file.
    let disk =
        std::fs::read_to_string(server.api.runtime.daemon_paths().browser_grants_file()).unwrap();
    assert!(!disk.contains(token));
    assert!(!disk.contains(&tab.credential_id));
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_proof_collects_an_unpolled_approval_and_an_abandoned_one_ends_at_its_link_deadline()
{
    let sandbox = support::sandbox(
        "daemon::browser_reauth::first_proof_collects_an_unpolled_approval_and_an_abandoned_one_ends_at_its_link_deadline",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    // Reload right after approval: the polling secret is lost, proof works
    // and a later poll (another tab) sees the link gone.
    let tab = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &tab).await;
    let (code, recovered) = prove(&server, &sandbox, ORIGIN, &tab, &saved).await;
    assert_eq!(code, 200, "{recovered}");
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 410);
    // Approved, never collected, link deadline passed with no other link
    // created to sweep it: proof fails generically and the grant ends.
    let abandoned = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &abandoned).await;
    let late = chrono::DateTime::parse_from_rfc3339(created["expires_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc)
        + chrono::Duration::seconds(1);
    let context = ctx(ORIGIN, "component");
    let issued = challenge_at(&server, &context, &abandoned, &saved, late, Instant::now());
    assert_eq!(
        complete_at(
            &server,
            &context,
            &issued,
            abandoned.sign(&issued),
            late,
            Instant::now()
        )
        .unwrap_err(),
        BrowserError::ReauthFailed
    );
    assert_eq!(browser::browsers_list(&server.api).items.len(), 1);
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proof_after_expiry_reports_authorization_expired_on_either_clock_and_never_extends() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::proof_after_expiry_reports_authorization_expired_on_either_clock_and_never_extends",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let context = ctx(ORIGIN, "component");
    let twelve_hours = Duration::from_secs(12 * 60 * 60);
    // Wall clock past the original expiry (e.g. after a suspend).
    let wall = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &wall).await;
    let (_, polled) = poll(&server, ORIGIN, &created).await;
    let token = polled["grant"]["token"].as_str().unwrap().to_owned();
    sandbox.redact(&token);
    let grant_id = BrowserGrantId::parse(polled["grant"]["id"].as_str().unwrap()).unwrap();
    let watch = server.api.runtime.browser_auth().watch(&grant_id).unwrap();
    let after = chrono::Utc::now() + chrono::Duration::from_std(twelve_hours).unwrap();
    // A live socket's watch observes either clock without a timer firing.
    assert_eq!(watch.check(chrono::Utc::now(), Instant::now()), None);
    assert_eq!(
        watch.check(after, Instant::now()),
        Some(GrantRevocationReason::Expired)
    );
    assert_eq!(
        watch.check(chrono::Utc::now(), Instant::now() + twelve_hours),
        Some(GrantRevocationReason::Expired)
    );
    let issued = challenge_at(&server, &context, &wall, &saved, after, Instant::now());
    assert_eq!(
        complete_at(
            &server,
            &context,
            &issued,
            wall.sign(&issued),
            after,
            Instant::now()
        )
        .unwrap_err(),
        BrowserError::AuthorizationExpired
    );
    // Expired is final, even back on the current clock; no token survives.
    let (code, body) = prove(&server, &sandbox, ORIGIN, &wall, &saved).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (403, Some("authorization_expired"))
    );
    assert_eq!(
        server
            .request("GET", "/v1/status", Some(&token), Some(ORIGIN), None)
            .await
            .0,
        401
    );
    assert!(watch.is_revoked());
    // Monotonic deadline passed while the wall clock was rolled back.
    let mono = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &mono).await;
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 200);
    let rolled_back = chrono::Utc::now() - chrono::Duration::hours(1);
    let later = Instant::now() + twelve_hours;
    let issued = challenge_at(&server, &context, &mono, &saved, rolled_back, later);
    assert_eq!(
        complete_at(
            &server,
            &context,
            &issued,
            mono.sign(&issued),
            rolled_back,
            later
        )
        .unwrap_err(),
        BrowserError::AuthorizationExpired
    );
    // A wrong key against the expired tombstone learns nothing.
    let (code, body) = prove(&server, &sandbox, ORIGIN, &forged(&mono), &saved).await;
    assert_eq!((code, body["code"].as_str()), (401, Some("reauth_failed")));
    server.shutdown().await;
    sandbox.finish();
}

/// Another key presenting `browser`'s credential ID.
fn forged(browser: &Browser) -> Browser {
    Browser {
        credential_id: browser.credential_id.clone(),
        ..Browser::new()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_between_challenge_and_completion_reports_revoked_and_purges_access() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::revocation_between_challenge_and_completion_reports_revoked_and_purges_access",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &tab).await;
    let (_, polled) = poll(&server, ORIGIN, &created).await;
    let token = polled["grant"]["token"].as_str().unwrap().to_owned();
    sandbox.redact(&token);
    let grant_id = polled["grant"]["id"].as_str().unwrap().to_owned();
    let id = BrowserGrantId::parse(&grant_id).unwrap();
    let auth = server.api.runtime.browser_auth();
    let watch = auth.watch(&id).unwrap();
    let (code, issued) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    assert_eq!(code, 201);
    browser::browsers_delete(&server.api, &grant_id).unwrap();
    assert_eq!(watch.ended().await, GrantRevocationReason::Deleted);
    assert_eq!(auth.tickets_for_grant(&id), 0);
    let (code, body) = complete(&server, ORIGIN, &issued["challenge_id"], &tab.sign(&issued)).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (403, Some("authorization_revoked"))
    );
    assert!(body.get("grant").is_none());
    // Fresh challenge, same answer; a wrong key still gets the generic one.
    let (code, body) = prove(&server, &sandbox, ORIGIN, &tab, &saved).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (403, Some("authorization_revoked"))
    );
    let (code, body) = prove(&server, &sandbox, ORIGIN, &forged(&tab), &saved).await;
    assert_eq!((code, body["code"].as_str()), (401, Some("reauth_failed")));
    assert_eq!(
        server
            .request("GET", "/v1/status", Some(&token), Some(ORIGIN), None)
            .await
            .0,
        401
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_restart_and_listener_generation_changes_invalidate_saved_proofs() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::daemon_restart_and_listener_generation_changes_invalidate_saved_proofs",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &tab).await;
    let (_, polled) = poll(&server, ORIGIN, &created).await;
    let token = polled["grant"]["token"].as_str().unwrap().to_owned();
    sandbox.redact(&token);
    // `engine` removed and re-added without a rebind: every grant ended
    // and the listener's generation changed.
    let path =
        awman::data::config::file::global_config_path(&support::env_snapshot(&sandbox)).unwrap();
    let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut without_engine = original.clone();
    merge(
        &mut without_engine,
        json!({"daemon":{"network":{"listeners":{"component":{"features":[]}}}}}),
    );
    std::fs::write(&path, without_engine.to_string()).unwrap();
    server.api.runtime.reload_config().await.unwrap();
    std::fs::write(&path, original.to_string()).unwrap();
    server.api.runtime.reload_config().await.unwrap();
    let rotated = epochs(&server, ORIGIN).await;
    assert_eq!(rotated.daemon, saved.daemon);
    assert_ne!(rotated.listener, saved.listener);
    let (code, body) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (409, Some("listener_changed"))
    );
    let (code, body) = prove(&server, &sandbox, ORIGIN, &tab, &rotated).await;
    assert_eq!((code, body["code"].as_str()), (401, Some("reauth_failed")));
    // Daemon restart on the same data directory: new daemon epoch, old
    // proofs and bearers carry no authority.
    let second = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &second).await;
    let (_, polled) = poll(&server, ORIGIN, &created).await;
    let second_token = polled["grant"]["token"].as_str().unwrap().to_owned();
    sandbox.redact(&second_token);
    server.shutdown().await;
    let server = Api::restart(&sandbox).await;
    let restarted = epochs(&server, ORIGIN).await;
    assert_ne!(restarted.daemon, rotated.daemon);
    let (code, body) = challenge(&server, ORIGIN, &second.credential_id, &rotated).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (409, Some("daemon_restarted"))
    );
    // Daemon is checked before listener.
    let mixed = Epochs {
        daemon: rotated.daemon.clone(),
        listener: restarted.listener.clone(),
    };
    let (code, body) = challenge(&server, ORIGIN, &second.credential_id, &mixed).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (409, Some("daemon_restarted"))
    );
    let (code, body) = prove(&server, &sandbox, ORIGIN, &second, &restarted).await;
    assert_eq!((code, body["code"].as_str()), (401, Some("reauth_failed")));
    assert_eq!(
        server
            .request("GET", "/v1/status", Some(&second_token), Some(ORIGIN), None)
            .await
            .0,
        401
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_mismatch_is_refused_generically_or_at_the_gate() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::origin_mismatch_is_refused_generically_or_at_the_gate",
    );
    let server = Api::with_config(
        &sandbox,
        json!({"daemon":{"network":{"listeners":{"component":{"allowedOrigins":[ORIGIN, OTHER_ORIGIN]}}}}}),
    )
    .await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &tab).await;
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 200);
    // Another allowed origin gets the ordinary challenge shape with its own
    // origin in it; a correct signature still cannot release the grant.
    let (code, issued) = challenge(&server, OTHER_ORIGIN, &tab.credential_id, &saved).await;
    assert_eq!(code, 201);
    assert_eq!(issued["origin"], OTHER_ORIGIN);
    let (code, body) = complete(
        &server,
        OTHER_ORIGIN,
        &issued["challenge_id"],
        &tab.sign(&issued),
    )
    .await;
    assert_eq!((code, body["code"].as_str()), (401, Some("reauth_failed")));
    // A challenge issued to the right origin, completed from another one,
    // fails and is consumed.
    let (_, issued) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    let signature = tab.sign(&issued);
    assert_eq!(
        complete(&server, OTHER_ORIGIN, &issued["challenge_id"], &signature)
            .await
            .0,
        401
    );
    assert_eq!(
        complete(&server, ORIGIN, &issued["challenge_id"], &signature)
            .await
            .0,
        401
    );
    // Disallowed, null and missing Origin never reach proof handling.
    for origin in ["https://evil.invalid", "null"] {
        let (code, body) = challenge(&server, origin, &tab.credential_id, &saved).await;
        assert_eq!(
            (code, body["code"].as_str()),
            (403, Some("origin_not_allowed"))
        );
    }
    // API keys never reach the browser-only recovery routes.
    let key = awman::command::daemon_api::keys::keys_create(&server.api, "non-browser").unwrap();
    sandbox.redact(&key.key);
    let (code, _) = server
        .request(
            "POST",
            "/v1/browser-reauth/challenge",
            Some(&key.key),
            None,
            Some(json!({})),
        )
        .await;
    assert_eq!(code, 404, "non-browser callers cannot see the route");
    // The right origin still recovers.
    assert_eq!(prove(&server, &sandbox, ORIGIN, &tab, &saved).await.0, 200);
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_mismatch_fails_generically_even_with_a_valid_signature() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::listener_mismatch_fails_generically_even_with_a_valid_signature",
    );
    let other = sandbox.ports().tcp_listener();
    let server = Api::with_config(
        &sandbox,
        json!({"daemon":{"network":{"listeners":{"second":{"enabled":true,
            "listen":other.local_addr().unwrap().to_string(),"transport":"loopback_http",
            "features":["engine"],"allowedOrigins":[ORIGIN]}}}}}),
    )
    .await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &tab).await;
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 200);
    let here = ctx(ORIGIN, "component");
    let there = ctx(ORIGIN, "second");
    let there_epochs = Epochs {
        daemon: saved.daemon.clone(),
        listener: server.api.runtime.browser_auth().listener_epoch("second"),
    };
    let now = chrono::Utc::now();
    // Proof at another eligible listener, with its own valid challenge.
    let issued = challenge_at(&server, &there, &tab, &there_epochs, now, Instant::now());
    assert_eq!(issued["listener"], "second");
    assert_eq!(
        complete_at(
            &server,
            &there,
            &issued,
            tab.sign(&issued),
            now,
            Instant::now()
        )
        .unwrap_err(),
        BrowserError::ReauthFailed
    );
    // A challenge copied from the right listener to another one.
    let issued = challenge_at(&server, &here, &tab, &saved, now, Instant::now());
    let signature = tab.sign(&issued);
    assert_eq!(
        complete_at(
            &server,
            &there,
            &issued,
            signature.clone(),
            now,
            Instant::now()
        )
        .unwrap_err(),
        BrowserError::ReauthFailed
    );
    assert_eq!(
        complete_at(&server, &here, &issued, signature, now, Instant::now()).unwrap_err(),
        BrowserError::ReauthFailed,
        "the copied challenge was consumed"
    );
    // The wrong listener's epoch is refused before lookup.
    let wrong = Epochs {
        daemon: saved.daemon.clone(),
        listener: there_epochs.listener.clone(),
    };
    let (code, body) = challenge(&server, ORIGIN, &tab.credential_id, &wrong).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (409, Some("listener_changed"))
    );
    drop(other);
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_used_challenge_cannot_be_replayed_concurrently_malleated_or_after_bad_encoding() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::a_used_challenge_cannot_be_replayed_concurrently_malleated_or_after_bad_encoding",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &tab).await;
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 200);
    // Sequential replay of an accepted completion.
    let (_, issued) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    let signature = tab.sign(&issued);
    let (code, body) = complete(&server, ORIGIN, &issued["challenge_id"], &signature).await;
    assert_eq!(code, 200);
    sandbox.redact(body["grant"]["token"].as_str().unwrap());
    let (code, body) = complete(&server, ORIGIN, &issued["challenge_id"], &signature).await;
    assert_eq!((code, body["code"].as_str()), (401, Some("reauth_failed")));
    // Concurrent identical completions: exactly one release.
    let (_, issued) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    let signature = tab.sign(&issued);
    let (a, b) = tokio::join!(
        complete(&server, ORIGIN, &issued["challenge_id"], &signature),
        complete(&server, ORIGIN, &issued["challenge_id"], &signature)
    );
    for (_, body) in [&a, &b] {
        if let Some(token) = body["grant"]["token"].as_str() {
            sandbox.redact(token);
        }
    }
    let mut codes = [a.0, b.0];
    codes.sort_unstable();
    assert_eq!(codes, [200, 401]);
    // A malformed signature consumes the challenge (400); the valid one
    // afterwards fails generically.
    let (_, issued) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    let (code, body) = complete(&server, ORIGIN, &issued["challenge_id"], "not-base64url!").await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("invalid_request"))
    );
    assert_eq!(
        complete(&server, ORIGIN, &issued["challenge_id"], &tab.sign(&issued))
            .await
            .0,
        401
    );
    // Not structurally valid (extra field, wrong version): nothing consumed.
    let (_, issued) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    let signature = tab.sign(&issued);
    for body in [
        json!({"version":1,"challenge_id":issued["challenge_id"],"signature":signature,"extra":1}),
        json!({"version":2,"challenge_id":issued["challenge_id"],"signature":signature}),
    ] {
        let (code, _) = server
            .request(
                "POST",
                "/v1/browser-reauth/complete",
                None,
                Some(ORIGIN),
                Some(body),
            )
            .await;
        assert_eq!(code, 400);
    }
    let (code, body) = complete(&server, ORIGIN, &issued["challenge_id"], &signature).await;
    assert_eq!(code, 200);
    sandbox.redact(body["grant"]["token"].as_str().unwrap());
    // A nonce-tampered transcript is a different message.
    let (_, mut issued) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    let id = issued["challenge_id"].clone();
    issued["nonce"] = json!(random_id(32));
    assert_eq!(
        complete(&server, ORIGIN, &id, &tab.sign(&issued)).await.0,
        401
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_keys_and_unknown_credentials_fail_identically_without_locking_recovery() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::wrong_keys_and_unknown_credentials_fail_identically_without_locking_recovery",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &tab).await;
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 200);
    // Same challenge shape for known and unknown credential IDs.
    let stranger = Browser::new();
    let (_, known) = challenge(&server, ORIGIN, &tab.credential_id, &saved).await;
    let (_, unknown) = challenge(&server, ORIGIN, &stranger.credential_id, &saved).await;
    let keys = |v: &Value| {
        let mut k: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(&known), keys(&unknown));
    let (code, body) = complete(
        &server,
        ORIGIN,
        &unknown["challenge_id"],
        &stranger.sign(&unknown),
    )
    .await;
    let unknown_answer = (code, body);
    // Ten proofs by an attacker's key for the real credential ID.
    let attacker = forged(&tab);
    for _ in 0..10 {
        let answer = prove(&server, &sandbox, ORIGIN, &attacker, &saved).await;
        assert_eq!(answer, unknown_answer);
        assert_eq!(answer.0, 401);
        assert_eq!(
            answer.1["error"],
            "Browser reauthorization proof was not accepted."
        );
    }
    // A key cannot be rebound to an existing grant by enrolling its ID.
    let (code, _) = server
        .request(
            "POST",
            "/v1/browser-links",
            None,
            Some(ORIGIN),
            Some(attacker.enrollment()),
        )
        .await;
    assert_eq!(code, 409);
    // Malformed enrollment keys are invalid requests.
    let mut bad = Browser::new().enrollment();
    bad["reauth"]["public_key"] = json!(b64(&[4u8; 65]));
    let (code, body) = server
        .request("POST", "/v1/browser-links", None, Some(ORIGIN), Some(bad))
        .await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("invalid_request"))
    );
    // The real key is not locked out.
    assert_eq!(prove(&server, &sandbox, ORIGIN, &tab, &saved).await.0, 200);
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_tabs_recover_one_grant_concurrently_without_rotating_each_other() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::two_tabs_recover_one_grant_concurrently_without_rotating_each_other",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let profile = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &profile).await;
    // Both tabs reload while the link is still uncollected: each proves
    // independently; exactly one collection, both get the same grant.
    let (first, second) = tokio::join!(
        prove(&server, &sandbox, ORIGIN, &profile, &saved),
        prove(&server, &sandbox, ORIGIN, &profile, &saved)
    );
    assert_eq!((first.0, second.0), (200, 200), "{first:?} {second:?}");
    assert_eq!(first.1["grant"], second.1["grant"]);
    assert_eq!(poll(&server, ORIGIN, &created).await.0, 410);
    let token = first.1["grant"]["token"].as_str().unwrap();
    // Each tab's bearer and tickets keep working after the other recovers.
    for _ in 0..2 {
        assert_eq!(
            server
                .request("GET", "/v1/status", Some(token), Some(ORIGIN), None)
                .await
                .0,
            200
        );
        assert_eq!(
            prove(&server, &sandbox, ORIGIN, &profile, &saved).await.0,
            200
        );
    }
    assert_eq!(browser::browsers_list(&server.api).items.len(), 1);
    server.shutdown().await;
    sandbox.finish();
}

// ─── Security review regressions (SR-02 – SR-07, SR-10) ─────────────────────
//
// Each inverts a reproduction from the WI 0160 implementation review. Time is
// controlled with the grant store's `ManualClock`; no machine clock changes.

use awman::command::daemon_api::browser::BrowserPrincipal;
use awman::command::instance::admission::AttachRefusal;
use awman::data::browser_auth::{BrowserTicketRequest, TicketRoute};
use awman::engine::auth::browser_grants::{GrantClock, GrantVerdict, ManualClock, SamplePoint};

/// Install a manual grant clock before any grant exists.
fn manual_clock(server: &Api) -> std::sync::Arc<ManualClock> {
    let clock = ManualClock::new();
    server
        .api
        .runtime
        .browser_auth()
        .grants()
        .set_clock(clock.clone());
    clock
}

/// Link, approve and poll a key-bound browser: (grant ID, bearer).
async fn linked(
    server: &Api,
    sandbox: &awman_test_support::TestSandbox,
    browser: &Browser,
) -> (BrowserGrantId, String) {
    let created = link_and_approve(server, sandbox, ORIGIN, browser).await;
    let (code, polled) = poll(server, ORIGIN, &created).await;
    assert_eq!(code, 200, "{polled}");
    let token = polled["grant"]["token"].as_str().unwrap().to_owned();
    sandbox.redact(&token);
    let id = BrowserGrantId::parse(polled["grant"]["id"].as_str().unwrap()).unwrap();
    (id, token)
}

async fn status_with(server: &Api, token: &str) -> u16 {
    server
        .request("GET", "/v1/status", Some(token), Some(ORIGIN), None)
        .await
        .0
}

fn web_primary_hello() -> awman::data::frontend_protocol::Hello {
    use awman::data::frontend_protocol::*;
    Hello {
        protocol: FRONTEND_SUBPROTOCOL.to_string(),
        build: "test".into(),
        kind: FrontendKindWire::Web,
        role: FrontendRole::Primary,
        annotations: awman::data::annotations::Annotations::new(),
        cwd: Default::default(),
        argv: vec![],
        env: Default::default(),
        launch_context: Some(awman::data::ids::LaunchContextId::new()),
        term_size: TermSize { cols: 80, rows: 24 },
        stdin_is_tty: false,
        stdout_is_tty: false,
        view: None,
        resume: None,
    }
}

/// The production host's admission, as the frontend socket calls it.
async fn admit_web_primary(
    server: &Api,
    instance: &awman::data::ids::InstanceId,
    grant: awman::engine::auth::browser_grants::GrantWatch,
) -> Result<(), AttachRefusal> {
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    server
        .api
        .runtime
        .engine_host()
        .attach_frontend_as(instance.clone(), web_primary_hello(), tx, Some(grant))
        .await
        .map(|_| ())
}

fn primary_kind(
    server: &Api,
    instance: &awman::data::ids::InstanceId,
) -> Option<awman::data::frontend_protocol::FrontendKindWire> {
    server
        .api
        .runtime
        .engine_host()
        .get(instance)
        .and_then(|d| d.summary.primary_frontend_kind)
}

/// SR-02: the first observed expiry is final whichever path observed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observed_expiry_is_final_on_every_path_after_the_clock_is_corrected_back() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::an_observed_expiry_is_final_on_every_path_after_the_clock_is_corrected_back",
    );
    let server = Api::start(&sandbox).await;
    let clock = manual_clock(&server);
    let saved = epochs(&server, ORIGIN).await;
    let auth = server.api.runtime.browser_auth();
    let context = ctx(ORIGIN, "component");
    let thirteen = chrono::Duration::hours(13);
    for path in ["bearer", "ticket", "watch", "proof", "admission"] {
        // A fresh minute for the link-create limiter.
        clock.advance(Duration::from_secs(61));
        let tab = Browser::new();
        let (id, token) = linked(&server, &sandbox, &tab).await;
        let watch = auth.watch(&id).unwrap();
        let instance = server
            .api
            .runtime
            .engine_host()
            .create_instance(Default::default())
            .unwrap();
        auth.record_instance(&id, &instance);
        let principal = BrowserPrincipal {
            grant_id: id.clone(),
            origin: ORIGIN.into(),
            listener: "component".into(),
        };
        clock.jump_wall(thirteen);
        match path {
            "bearer" => assert_eq!(status_with(&server, &token).await, 401),
            "ticket" => {
                let issued = browser::tickets_create(
                    &server.api,
                    &principal,
                    &BrowserTicketRequest::Frontend {
                        instance_id: instance.clone(),
                    },
                )
                .unwrap();
                sandbox.redact(issued.ticket.expose());
                assert!(auth
                    .consume_ticket(
                        issued.ticket.expose(),
                        TicketRoute::Frontend,
                        instance.as_str(),
                        &context
                    )
                    .is_none());
            }
            "watch" => assert_eq!(watch.check_now(), Some(GrantRevocationReason::Expired)),
            "proof" => {
                let (code, body) = prove(&server, &sandbox, ORIGIN, &tab, &saved).await;
                assert_eq!(
                    (code, body["code"].as_str()),
                    (403, Some("authorization_expired"))
                );
            }
            _ => assert_eq!(
                admit_web_primary(&server, &instance, watch.clone()).await,
                Err(AttachRefusal::GrantEnded(GrantRevocationReason::Expired))
            ),
        }
        // The wall clock is corrected back: nothing revives the grant.
        clock.jump_wall(-thirteen);
        let (now, _) = clock.sample(SamplePoint::Ordinary);
        assert_eq!(
            auth.grants()
                .verify(Some(&format!("Bearer {token}")), ORIGIN, "component", now),
            GrantVerdict::Invalid,
            "{path}"
        );
        assert_eq!(status_with(&server, &token).await, 401, "{path}");
        assert_eq!(
            watch.check_now(),
            Some(GrantRevocationReason::Expired),
            "{path}"
        );
        assert!(auth.watch(&id).is_none(), "{path}");
        let (code, body) = prove(&server, &sandbox, ORIGIN, &tab, &saved).await;
        assert_eq!(
            (code, body["code"].as_str()),
            (403, Some("authorization_expired")),
            "{path}"
        );
        assert!(body.get("grant").is_none());
        assert_eq!(auth.tickets_for_grant(&id), 0, "{path}");
        assert!(
            browser::browsers_list(&server.api)
                .items
                .iter()
                .all(|g| g.id != id),
            "{path}"
        );
        assert_eq!(primary_kind(&server, &instance), None, "{path}");
    }
    server.shutdown().await;
    sandbox.finish();
}

/// SR-03 / SR-10: revocation or expiry after the upgrade and before
/// admission refuses at the daemon host's admission, with no primary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_ended_before_admission_admits_nothing_on_the_daemon_host() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::a_grant_ended_before_admission_admits_nothing_on_the_daemon_host",
    );
    let server = Api::start(&sandbox).await;
    let clock = manual_clock(&server);
    let auth = server.api.runtime.browser_auth();
    let host = server.api.runtime.engine_host();
    // Revoked by the owner after the ticket was consumed.
    let (id, _) = linked(&server, &sandbox, &Browser::new()).await;
    let instance = host.create_instance(Default::default()).unwrap();
    let watch = auth.watch(&id).unwrap();
    browser::browsers_delete(&server.api, id.as_str()).unwrap();
    assert_eq!(
        admit_web_primary(&server, &instance, watch).await,
        Err(AttachRefusal::GrantEnded(GrantRevocationReason::Deleted))
    );
    assert_eq!(primary_kind(&server, &instance), None);
    assert!(
        host.get(&instance).is_some(),
        "the reservation is untouched"
    );
    // Expired exactly at the admission check (a suspend while waiting).
    let tab = Browser::new();
    let saved = epochs(&server, ORIGIN).await;
    let (id, _) = linked(&server, &sandbox, &tab).await;
    let watch = auth.watch(&id).unwrap();
    clock.jump_at(
        SamplePoint::Admission,
        chrono::Duration::hours(13),
        Duration::ZERO,
    );
    assert_eq!(
        admit_web_primary(&server, &instance, watch).await,
        Err(AttachRefusal::GrantEnded(GrantRevocationReason::Expired))
    );
    assert_eq!(primary_kind(&server, &instance), None);
    clock.jump_wall(-chrono::Duration::hours(13));
    let (code, body) = prove(&server, &sandbox, ORIGIN, &tab, &saved).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (403, Some("authorization_expired"))
    );
    server.shutdown().await;
    sandbox.finish();
}

/// SR-04: deadlines are re-checked at release time, sampled after the
/// signature was verified (proof) or under the lock (poll).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadlines_crossed_during_proof_or_poll_are_enforced_at_release() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::deadlines_crossed_during_proof_or_poll_are_enforced_at_release",
    );
    let server = Api::start(&sandbox).await;
    let clock = manual_clock(&server);
    let saved = epochs(&server, ORIGIN).await;
    let context = ctx(ORIGIN, "component");
    let now = || clock.sample(SamplePoint::Ordinary);
    // Grant deadline crossed between verification and release.
    let tab = Browser::new();
    let (id, token) = linked(&server, &sandbox, &tab).await;
    // Ten seconds before the grant deadline; the 30 s challenge outlives it.
    let ttl = awman::data::browser_auth::GRANT_TTL;
    clock.advance(ttl - Duration::from_secs(10));
    let (t, i) = now();
    let issued = challenge_at(&server, &context, &tab, &saved, t, i);
    clock.jump_at(
        SamplePoint::ProofRelease,
        chrono::Duration::seconds(20),
        Duration::from_secs(20),
    );
    assert_eq!(
        complete_at(&server, &context, &issued, tab.sign(&issued), t, i).unwrap_err(),
        BrowserError::AuthorizationExpired
    );
    assert_eq!(status_with(&server, &token).await, 401);
    assert!(server.api.runtime.browser_auth().watch(&id).is_none());
    // The same crossing through the two explicit halves.
    clock.advance(Duration::from_secs(61));
    let other = Browser::new();
    linked(&server, &sandbox, &other).await;
    clock.advance(ttl - Duration::from_secs(10));
    let (t, i) = now();
    let issued = challenge_at(&server, &context, &other, &saved, t, i);
    let proof = browser::reauth_verify_at(
        &server.api,
        &context,
        &BrowserReauthCompleteRequest {
            version: 1,
            challenge_id: issued["challenge_id"].as_str().unwrap().to_owned(),
            signature: BrowserSecret::new(other.sign(&issued)),
        },
        t,
        i,
    )
    .unwrap();
    clock.advance(Duration::from_secs(20));
    assert_eq!(
        browser::reauth_release(&server.api, &context, proof).unwrap_err(),
        BrowserError::AuthorizationExpired
    );
    // Challenge deadline crossed during the operation: generic failure,
    // and the grant itself stays recoverable with a fresh challenge.
    clock.advance(Duration::from_secs(61));
    let tab = Browser::new();
    linked(&server, &sandbox, &tab).await;
    let (t, i) = now();
    let issued = challenge_at(&server, &context, &tab, &saved, t, i);
    clock.jump_at(
        SamplePoint::ProofRelease,
        chrono::Duration::seconds(31),
        Duration::from_secs(31),
    );
    assert_eq!(
        complete_at(&server, &context, &issued, tab.sign(&issued), t, i).unwrap_err(),
        BrowserError::ReauthFailed
    );
    assert_eq!(prove(&server, &sandbox, ORIGIN, &tab, &saved).await.0, 200);
    // Uncollected approval whose link deadline passes during the proof: it
    // ends instead of being collected late.
    clock.advance(Duration::from_secs(61));
    let abandoned = Browser::new();
    link_and_approve(&server, &sandbox, ORIGIN, &abandoned).await;
    let before = browser::browsers_list(&server.api).items.len();
    // Ten seconds before the five-minute link deadline.
    clock.advance(awman::data::browser_auth::LINK_TTL - Duration::from_secs(10));
    let (t, i) = now();
    let issued = challenge_at(&server, &context, &abandoned, &saved, t, i);
    clock.jump_at(
        SamplePoint::ProofRelease,
        chrono::Duration::seconds(20),
        Duration::from_secs(20),
    );
    assert_eq!(
        complete_at(&server, &context, &issued, abandoned.sign(&issued), t, i).unwrap_err(),
        BrowserError::ReauthFailed
    );
    assert_eq!(browser::browsers_list(&server.api).items.len(), before - 1);
    // Poll: the grant expires between the request and its collection. The
    // link is consumed and no bearer is released.
    clock.advance(Duration::from_secs(61));
    let polled = Browser::new();
    let created = link_and_approve(&server, &sandbox, ORIGIN, &polled).await;
    let (t, i) = now();
    // The monotonic deadline passes (the link's wall-clock one does not).
    clock.jump_at(
        SamplePoint::PollRelease,
        chrono::Duration::zero(),
        awman::data::browser_auth::GRANT_TTL,
    );
    assert_eq!(
        browser::links_poll_at(
            &server.api,
            &context,
            created["id"].as_str().unwrap(),
            created["polling_secret"].as_str().unwrap(),
            t,
            i
        )
        .unwrap_err(),
        BrowserError::LinkGone
    );
    let (code, body) = prove(&server, &sandbox, ORIGIN, &polled, &saved).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (403, Some("authorization_expired"))
    );
    server.shutdown().await;
    sandbox.finish();
}

/// SR-05: expiry → another approval → a valid proof still learns
/// `authorization_expired`, and expiry cleanup needs no new link traffic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_grant_keeps_its_reason_after_another_approval_and_is_cleaned_up_unprompted() {
    let sandbox = support::sandbox(
        "daemon::browser_reauth::an_expired_grant_keeps_its_reason_after_another_approval_and_is_cleaned_up_unprompted",
    );
    let server = Api::start(&sandbox).await;
    let clock = manual_clock(&server);
    let saved = epochs(&server, ORIGIN).await;
    let auth = server.api.runtime.browser_auth();
    let context = ctx(ORIGIN, "component");
    // The review's counterexample: another grant created at a later time.
    let first = Browser::new();
    linked(&server, &sandbox, &first).await;
    let after = clock.sample(SamplePoint::Ordinary).0 + chrono::Duration::hours(13);
    auth.grants()
        .create(
            ORIGIN,
            "component",
            awman::data::browser_auth::BrowserScope::Engine,
            None,
            after,
        )
        .unwrap();
    let issued = challenge_at(&server, &context, &first, &saved, after, Instant::now());
    assert_eq!(
        complete_at(
            &server,
            &context,
            &issued,
            first.sign(&issued),
            after,
            Instant::now()
        )
        .unwrap_err(),
        BrowserError::AuthorizationExpired
    );
    // Through the production approval path, after a suspend.
    clock.advance(Duration::from_secs(61));
    let a = Browser::new();
    let (a_id, a_token) = linked(&server, &sandbox, &a).await;
    clock.jump_wall(chrono::Duration::hours(13));
    clock.advance(Duration::from_secs(61));
    linked(&server, &sandbox, &Browser::new()).await;
    let (code, body) = prove(&server, &sandbox, ORIGIN, &a, &saved).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (403, Some("authorization_expired"))
    );
    assert_eq!(status_with(&server, &a_token).await, 401);
    assert!(auth.watch(&a_id).is_none());
    // No traffic at all: the periodic sweep ends an expired grant, its
    // tickets and its retained bearer; the tombstone answers later.
    let quiet = Browser::new();
    clock.advance(Duration::from_secs(61));
    let (q_id, _) = linked(&server, &sandbox, &quiet).await;
    let instance = server
        .api
        .runtime
        .engine_host()
        .create_instance(Default::default())
        .unwrap();
    auth.record_instance(&q_id, &instance);
    let principal = BrowserPrincipal {
        grant_id: q_id.clone(),
        origin: ORIGIN.into(),
        listener: "component".into(),
    };
    let ticket = browser::tickets_create(
        &server.api,
        &principal,
        &BrowserTicketRequest::Frontend {
            instance_id: instance.clone(),
        },
    )
    .unwrap();
    sandbox.redact(ticket.ticket.expose());
    assert_eq!(auth.tickets_for_grant(&q_id), 1);
    clock.jump_wall(chrono::Duration::hours(13));
    auth.reap_expired();
    assert_eq!(auth.tickets_for_grant(&q_id), 0);
    assert!(auth.grants().get(&q_id).is_none(), "the record is gone");
    assert!(!browser::visible_instance(
        &server.api,
        &principal,
        &server
            .api
            .runtime
            .engine_host()
            .get(&instance)
            .unwrap()
            .summary
    ));
    let (code, body) = prove(&server, &sandbox, ORIGIN, &quiet, &saved).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (403, Some("authorization_expired"))
    );
    server.shutdown().await;
    sandbox.finish();
}

/// SR-06: a bulk revocation whose disk write fails still reports its
/// grants, purges their tickets and records their tombstones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_revocation_write_still_tombstones_purges_tickets_and_reports() {
    use awman::command::daemon_runtime::network::{BrowserRevocation, BrowserRevocationSink};
    let sandbox = support::sandbox(
        "daemon::browser_reauth::a_failed_revocation_write_still_tombstones_purges_tickets_and_reports",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let auth = server.api.runtime.browser_auth();
    let tab = Browser::new();
    let (id, token) = linked(&server, &sandbox, &tab).await;
    let other = Browser::new();
    let (other_id, _) = linked(&server, &sandbox, &other).await;
    let instance = server
        .api
        .runtime
        .engine_host()
        .create_instance(Default::default())
        .unwrap();
    auth.record_instance(&id, &instance);
    let principal = BrowserPrincipal {
        grant_id: id.clone(),
        origin: ORIGIN.into(),
        listener: "component".into(),
    };
    let ticket = browser::tickets_create(
        &server.api,
        &principal,
        &BrowserTicketRequest::Frontend {
            instance_id: instance.clone(),
        },
    )
    .unwrap();
    sandbox.redact(ticket.ticket.expose());
    let watch = auth.watch(&id).unwrap();
    // Make only this sandbox's grant file unwritable.
    let path = auth.grants().path().to_owned();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    auth.revoke(
        "component",
        BrowserRevocation::OriginRemoved(serde_json::from_value(json!(ORIGIN)).unwrap()),
    );
    assert_eq!(watch.ended().await, GrantRevocationReason::OriginRemoved);
    assert_eq!(auth.tickets_for_grant(&id), 0);
    assert_eq!(auth.tickets_for_grant(&other_id), 0);
    // The policy call reports the IDs and the disk error separately.
    std::fs::remove_dir(&path).unwrap();
    let retry = Browser::new();
    let (late_id, _) = linked(&server, &sandbox, &retry).await;
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let report = auth.revoke_where(
        |o, l| o == ORIGIN && l == "component",
        GrantRevocationReason::OriginRemoved,
    );
    assert_eq!(report.ids, vec![late_id]);
    assert!(report.persisted.is_err());
    std::fs::remove_dir(&path).unwrap();
    for browser in [&tab, &other, &retry] {
        let (code, body) = prove(&server, &sandbox, ORIGIN, browser, &saved).await;
        assert_eq!(
            (code, body["code"].as_str()),
            (403, Some("authorization_revoked"))
        );
    }
    assert_eq!(status_with(&server, &token).await, 401);
    server.shutdown().await;
    sandbox.finish();
}

/// SR-07: a listener's address is restart-only for a live reload, so the
/// old bind and its grants stay as they were; and any replacement of a
/// listener's bind under the same name (the generation the transport runner
/// rebinds on) ends its grants and reauthorization generation, so an old
/// bearer or proof fails on the old and the new address alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_rebound_to_a_new_address_ends_its_grants_and_reauth_generation() {
    use awman::command::daemon_runtime::network::{
        browser_revocations_between, BrowserRevocation, BrowserRevocationSink,
    };
    let sandbox = support::sandbox(
        "daemon::browser_reauth::a_listener_rebound_to_a_new_address_ends_its_grants_and_reauth_generation",
    );
    let server = Api::start(&sandbox).await;
    let saved = epochs(&server, ORIGIN).await;
    let tab = Browser::new();
    let (id, token) = linked(&server, &sandbox, &tab).await;
    let auth = server.api.runtime.browser_auth();
    let watch = auth.watch(&id).unwrap();
    let rebound = server
        .serve_listener_on_new_address(&sandbox, "component")
        .await;
    // A live reload does not move the bind: restart required, nothing ends.
    let before = server.api.runtime.config().daemon().network;
    let response = server
        .set_listener_address(&sandbox, "component", &rebound.addr)
        .await;
    assert!(response
        .restart_required
        .contains(&"daemon.network.listeners.component.listen".to_string()));
    assert_eq!(server.api.runtime.config().daemon().network, before);
    assert!(!watch.is_revoked());
    assert_eq!(status_with(&server, &token).await, 200);
    // The same name on a new address is a new generation.
    let mut after = before.clone();
    after.listeners.get_mut("component").unwrap().listen =
        awman::data::config::network::ListenAddr::parse(&rebound.addr).unwrap();
    let changes = browser_revocations_between(&before, &after);
    assert_eq!(
        changes,
        vec![("component".to_string(), BrowserRevocation::Rebound)]
    );
    for (listener, reason) in changes {
        auth.revoke(&listener, reason);
    }
    assert_eq!(watch.ended().await, GrantRevocationReason::ListenerClosed);
    // Until a restart applies the new address, the listener accepts only
    // its configured Host, so nothing reaches it through the new address.
    let (code, body) = server
        .request_at(
            &rebound.base,
            "GET",
            "/v1/status",
            Some(&token),
            Some(ORIGIN),
            None,
        )
        .await;
    assert_eq!((code, body["code"].as_str()), (421, Some("host_mismatch")));
    {
        let base = &server.base;
        let (code, _) = server
            .request_at(base, "GET", "/v1/status", Some(&token), Some(ORIGIN), None)
            .await;
        assert_eq!(code, 401, "{base}");
        let (code, body) = server
            .request_at(
                base,
                "POST",
                "/v1/browser-reauth/challenge",
                None,
                Some(ORIGIN),
                Some(json!({"version":1,"credential_id":tab.credential_id,
                    "daemon_epoch":saved.daemon,"listener_epoch":saved.listener,
                    "client_nonce":random_id(32)})),
            )
            .await;
        assert_eq!(
            (code, body["code"].as_str()),
            (409, Some("listener_changed")),
            "{base}"
        );
    }
    let (code, health) = server
        .request_at(&server.base, "GET", "/v1/health", None, Some(ORIGIN), None)
        .await;
    assert_eq!(code, 200);
    assert_ne!(
        health["browser_reauth"]["listener_epoch"],
        json!(saved.listener)
    );
    rebound.stop().await;
    server.shutdown().await;
    sandbox.finish();
}
