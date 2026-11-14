//! Provider protocol tests: private snapshots, owned loopback listeners and fake gh.
use crate::support;
use awman::data::{
    config::{env::EnvSnapshot, work_items::WorkItemProviderConfig},
    work_item_ref::WorkItemRef,
};
use awman::engine::{
    git::GitEngine,
    issue::{
        GithubIssueSource, HttpIssueSource, Issue, IssueOperation, IssueSource, IssueSourceError,
        IssueSourceRouter,
    },
};
use awman_test_support::{fixtures::RepoBuilder, TestSandbox};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

async fn server(sandbox: &TestSandbox) -> MockServer {
    MockServer::builder()
        .listener(sandbox.ports().tcp_listener())
        .start()
        .await
}
fn private_env(sandbox: &TestSandbox) -> EnvSnapshot {
    support::env_snapshot(sandbox)
}
fn provider_config(base: &str) -> WorkItemProviderConfig {
    WorkItemProviderConfig {
        base_url: base.into(),
        token_env: "TRACKER_TOKEN".into(),
    }
}
fn http(base: &str, env: &EnvSnapshot) -> HttpIssueSource {
    HttpIssueSource::new("tracker", &provider_config(base), env).unwrap()
}
fn json(id: &str, body: &str) -> serde_json::Value {
    serde_json::json!({"id":id,"title":"A title","body":body,"number":42})
}
fn secret(sandbox: &mut TestSandbox) {
    sandbox.redact("test-secret-tracker");
    sandbox
        .env_mut()
        .set("TRACKER_TOKEN", "test-secret-tracker");
}
fn github(sandbox: &TestSandbox, command: PathBuf, base: &str) -> GithubIssueSource {
    let env = private_env(sandbox);
    GithubIssueSource::with_endpoints(
        Arc::new(GitEngine::with_env(env.clone())),
        env,
        command,
        base.parse().unwrap(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn http_reads_json_and_markdown_without_guessing_numbers() {
    let mut sandbox =
        support::sandbox("engine::issue::http_reads_json_and_markdown_without_guessing_numbers");
    secret(&mut sandbox);
    let server = server(&sandbox).await;
    Mock::given(method("GET"))
        .and(path("/items/PROJ-42"))
        .and(header("Authorization", "Bearer test-secret-tracker"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json("PROJ-42", "raw body")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/items/84"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "intro\n\n## First heading\n\nraw body",
            "text/markdown; charset=utf-8",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/items/no-heading"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("plain text", "text/markdown"))
        .expect(1)
        .mount(&server)
        .await;
    let base = format!("{}/items", server.uri());
    let env = private_env(&sandbox);
    tokio::task::spawn_blocking(move || {
        let provider = http(&base, &env);
        let issue = provider
            .fetch_issue(&format!("{base}/PROJ-42?private=query"), Path::new(""))
            .unwrap();
        assert_eq!(issue.id, "PROJ-42");
        assert_eq!(issue.number, Some(42));
        assert_eq!(issue.body, "raw body");
        assert_eq!(issue.provider, "http:tracker");
        assert!(issue.source_id.ends_with("?private=query"));
        assert!(!format!("{issue:?}").contains("query"));
        let issue = provider
            .fetch_issue(&format!("{base}/84"), Path::new(""))
            .unwrap();
        assert_eq!(issue.title, "First heading");
        assert_eq!(issue.number, None);
        assert!(issue.body.starts_with("intro"));
        assert_eq!(
            provider
                .fetch_issue(&format!("{base}/no-heading"), Path::new(""))
                .unwrap()
                .title,
            "Untitled work item"
        );
    })
    .await
    .unwrap();
    server.verify().await;
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_maps_auth_missing_permission_and_rate_limit_without_echoing_secrets() {
    let mut sandbox = support::sandbox(
        "engine::issue::http_maps_auth_missing_permission_and_rate_limit_without_echoing_secrets",
    );
    secret(&mut sandbox);
    let server = server(&sandbox).await;
    for (id, status) in [
        ("auth", 401),
        ("forbidden", 403),
        ("missing", 404),
        ("rate", 429),
    ] {
        Mock::given(path(format!("/items/{id}")))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string("test-secret-tracker private upstream text"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(path("/items/rate403"))
        .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"))
        .expect(1)
        .mount(&server)
        .await;
    let base = format!("{}/items", server.uri());
    let env = private_env(&sandbox);
    tokio::task::spawn_blocking(move || {
        let provider = http(&base, &env);
        for id in ["auth", "forbidden", "missing", "rate", "rate403"] {
            let error = provider
                .fetch_issue(
                    &format!("{base}/{id}?token=test-secret-query"),
                    Path::new(""),
                )
                .unwrap_err();
            assert!(!format!("{error:?} {error}").contains("test-secret"));
            match id {
                "auth" => assert!(matches!(error, IssueSourceError::Unauthorized { .. })),
                "forbidden" => assert!(matches!(error, IssueSourceError::Forbidden { .. })),
                "missing" => assert!(matches!(error, IssueSourceError::NotFound { .. })),
                _ => assert!(matches!(error, IssueSourceError::RateLimited { .. })),
            }
        }
    })
    .await
    .unwrap();
    server.verify().await;
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_redirects_and_unmatched_urls_never_receive_credentials() {
    let mut sandbox = support::sandbox(
        "engine::issue::http_redirects_and_unmatched_urls_never_receive_credentials",
    );
    secret(&mut sandbox);
    let first = server(&sandbox).await;
    let other = server(&sandbox).await;
    Mock::given(path("/items/redirect"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", format!("{}/stolen", other.uri())),
        )
        .expect(1)
        .mount(&first)
        .await;
    let base = format!("{}/items", first.uri());
    let unmatched = format!("{}/unconfigured", other.uri());
    let env = private_env(&sandbox);
    tokio::task::spawn_blocking(move || {
        let provider = http(&base, &env);
        assert!(provider
            .fetch_issue(&format!("{base}/redirect"), Path::new(""))
            .is_err());
        assert!(matches!(
            provider.fetch_issue(&unmatched, Path::new("")),
            Err(IssueSourceError::UnsafeUrl { .. })
        ));
        assert!(provider
            .fetch_issue(&format!("{base}-evil/42"), Path::new(""))
            .is_err());
        assert!(provider
            .fetch_issue("https://a.example.evil.com/items/42", Path::new(""))
            .is_err());
        assert!(
            HttpIssueSource::new("bad", &provider_config("http://a.example/items"), &env).is_err()
        );
    })
    .await
    .unwrap();
    assert!(
        other.received_requests().await.unwrap().is_empty(),
        "redirect or unmatched destination received a request"
    );
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
    first.verify().await;
    drop(first);
    drop(other);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_creates_encoded_ids_and_updates_only_the_original_identity() {
    let mut sandbox = support::sandbox(
        "engine::issue::http_creates_encoded_ids_and_updates_only_the_original_identity",
    );
    secret(&mut sandbox);
    let server = server(&sandbox).await;
    Mock::given(method("POST"))
        .and(path("/items"))
        .and(wiremock::matchers::body_json(
            serde_json::json!({"title":"-title","body":"first raw body"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json("PROJ/a:b", "first raw body")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/items/PROJ%2Fa%3Ab"))
        .and(wiremock::matchers::body_json(
            serde_json::json!({"body":"edited body"}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/items/original"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json("different", "changed")))
        .expect(1)
        .mount(&server)
        .await;
    let base = format!("{}/items", server.uri());
    let env = private_env(&sandbox);
    tokio::task::spawn_blocking(move || {
        let provider = http(&base, &env);
        let issue = provider
            .create_issue("-title", "first raw body", Path::new(""))
            .unwrap();
        assert!(issue.source_id.ends_with("/PROJ%2Fa%3Ab"));
        let updated = provider.update_issue(&issue, "edited body").unwrap();
        assert_eq!(updated.source_id, issue.source_id);
        assert_eq!(updated.id, issue.id);
        assert_eq!(updated.body, "edited body");
        let original = Issue {
            source_id: format!("{base}/original"),
            id: "original".into(),
            ..issue
        };
        assert!(provider.update_issue(&original, "body").is_err());
    })
    .await
    .unwrap();
    server.verify().await;
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_missing_tokens_and_invalid_inputs_make_no_requests() {
    let sandbox =
        support::sandbox("engine::issue::http_missing_tokens_and_invalid_inputs_make_no_requests");
    let server = server(&sandbox).await;
    let base = format!("{}/items", server.uri());
    let env = private_env(&sandbox);
    tokio::task::spawn_blocking(move || {
        let provider = http(&base, &env);
        assert!(matches!(
            provider.fetch_issue(&format!("{base}/42"), Path::new("")),
            Err(IssueSourceError::Unauthorized { .. })
        ));
        for suffix in ["/../outside", "/%252e%252e%252foutside", "/a%5Cb", "/%ZZ"] {
            assert!(matches!(
                provider.fetch_issue(&format!("{base}{suffix}"), Path::new("")),
                Err(IssueSourceError::UnsafeUrl { .. })
            ));
        }
    })
    .await
    .unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_indeterminate_creates_and_response_limits_are_typed() {
    let mut sandbox =
        support::sandbox("engine::issue::http_indeterminate_creates_and_response_limits_are_typed");
    secret(&mut sandbox);
    let server = server(&sandbox).await;
    Mock::given(method("POST"))
        .and(path("/items"))
        .respond_with(ResponseTemplate::new(201).set_body_string("malformed success"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/items/large"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(2 * 1024 * 1024 + 1)))
        .expect(1)
        .mount(&server)
        .await;
    let base = format!("{}/items", server.uri());
    let env = private_env(&sandbox);
    tokio::task::spawn_blocking(move || {
        let provider = http(&base, &env);
        assert!(matches!(
            provider.create_issue("t", "b", Path::new("")),
            Err(IssueSourceError::CreateOutcomeUnknown { .. })
        ));
        assert!(matches!(
            provider.fetch_issue(&format!("{base}/large"), Path::new("")),
            Err(IssueSourceError::ResponseTooLarge { .. })
        ));
    })
    .await
    .unwrap();
    server.verify().await;
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn parsed_router_rejects_bare_and_lookalike_urls_and_uses_specificity() {
    let sandbox = support::sandbox(
        "engine::issue::parsed_router_rejects_bare_and_lookalike_urls_and_uses_specificity",
    );
    let env = private_env(&sandbox);
    let providers = BTreeMap::from([
        ("wide".into(), provider_config("https://a.example/api")),
        (
            "narrow".into(),
            provider_config("https://a.example/api/items"),
        ),
    ]);
    let router =
        IssueSourceRouter::new(Arc::new(GitEngine::with_env(env.clone())), &env, &providers)
            .unwrap();
    assert_eq!(
        router
            .route(&WorkItemRef::parse("https://a.example/api/items/42").unwrap())
            .unwrap()
            .provider,
        "http:narrow"
    );
    assert!(router
        .route(&WorkItemRef::parse("https://a.example.evil.com/api/items/42").unwrap())
        .is_err());
    assert!(router.route(&WorkItemRef::parse("42").unwrap()).is_err());
    assert_eq!(
        router
            .route(&WorkItemRef::parse("github:42").unwrap())
            .unwrap()
            .provider,
        "github"
    );
    let mut providers = providers;
    providers.insert(
        "tied".into(),
        provider_config("https://a.example/api/items/"),
    );
    let router =
        IssueSourceRouter::new(Arc::new(GitEngine::with_env(env.clone())), &env, &providers)
            .unwrap();
    assert!(
        matches!(router.route(&WorkItemRef::parse("https://a.example/api/items/42").unwrap()), Err(IssueSourceError::AmbiguousProvider { providers }) if providers == ["http:narrow", "http:tied"])
    );
    // A definition the transport refuses (here, an encoded dot segment)
    // fails only references that could reach it.
    let providers = BTreeMap::from([
        ("good".into(), provider_config("https://a.example/api")),
        (
            "bad".into(),
            provider_config("https://b.example/x/%2e%2e/items"),
        ),
    ]);
    let router =
        IssueSourceRouter::new(Arc::new(GitEngine::with_env(env.clone())), &env, &providers)
            .unwrap();
    let route = |text: &str| router.route(&WorkItemRef::parse(text).unwrap());
    assert_eq!(route("github:42").unwrap().provider, "github");
    assert_eq!(route("http:good:7").unwrap().provider, "http:good");
    assert!(route("http:bad:7").is_err());
    assert!(router.http("bad").is_err());
    assert!(route("https://a.example/api/7").is_err());
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_cli_create_and_edit_use_stdin_and_literal_title() {
    let mut sandbox =
        support::sandbox("engine::issue::github_cli_create_and_edit_use_stdin_and_literal_title");
    // gh must not follow GH_HOST to a server other than the one awman reports.
    sandbox.env_mut().set("GH_HOST", "ghe.corp.example");
    sandbox.env_mut().set("GH_REPO", "other/repo");
    let server = server(&sandbox).await;
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("README.md", "fixture")
        .init();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue", "create"])
        .read_stdin()
        // gh may print a different owner/repo casing than origin uses.
        .stdout("https://github.com/Owner/Repo/issues/73\n");
    gh.on(&["issue", "edit"]).read_stdin();
    gh.on(&["issue","view"]).stdout(r#"{"number":73,"title":"fetched","body":"raw","url":"https://github.com/owner/repo/issues/73"}"#);
    let provider = github(&sandbox, gh.exe().into(), &server.uri());
    let root = repo.path().to_owned();
    tokio::task::spawn_blocking(move || {
        let issue = provider
            .create_issue("-dash-title", "first\nraw body", &root)
            .unwrap();
        assert_eq!(issue.number, Some(73));
        assert_eq!(issue.provider, "github");
        assert_eq!(issue.source_id, "https://github.com/owner/repo/issues/73");
        let updated = provider.update_issue(&issue, "edited\nraw body").unwrap();
        assert_eq!(updated.body, "edited\nraw body");
        assert_eq!(updated.source_id, issue.source_id);
        assert_eq!(
            provider.fetch_issue("owner/repo#73", &root).unwrap().body,
            "raw"
        );
    })
    .await
    .unwrap();
    let calls = gh.calls();
    for call in &calls {
        assert!(!call.env.contains_key("GH_HOST") && !call.env.contains_key("GH_REPO"));
    }
    assert_eq!(
        calls[0].argv,
        [
            "issue",
            "create",
            "--repo",
            "github.com/owner/repo",
            "--title=-dash-title",
            "--body-file",
            "-"
        ]
    );
    assert_eq!(calls[0].stdin, b"first\nraw body");
    assert_eq!(
        calls[1].argv,
        [
            "issue",
            "edit",
            "73",
            "--repo",
            "github.com/owner/repo",
            "--body-file",
            "-"
        ]
    );
    assert_eq!(calls[1].stdin, b"edited\nraw body");
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_rest_fallback_fetches_creates_and_updates_against_loopback() {
    let mut sandbox = support::sandbox(
        "engine::issue::github_rest_fallback_fetches_creates_and_updates_against_loopback",
    );
    sandbox.redact("test-secret-github");
    sandbox.env_mut().set("GITHUB_TOKEN", "test-secret-github");
    let server = server(&sandbox).await;
    let response = serde_json::json!({"number":73,"title":"-dash-title","body":"rest raw"});
    for (verb, endpoint) in [
        ("POST", "/repos/owner/repo/issues"),
        ("GET", "/repos/owner/repo/issues/73"),
        ("PATCH", "/repos/owner/repo/issues/73"),
    ] {
        Mock::given(method(verb))
            .and(path(endpoint))
            .and(header("Authorization", "Bearer test-secret-github"))
            .respond_with(
                ResponseTemplate::new(if verb == "POST" { 201 } else { 200 })
                    .set_body_json(&response),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("README.md", "fixture")
        .init();
    repo.git(&["remote", "add", "origin", "git@github.com:owner/repo.git"]);
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue"])
        .stderr("run gh auth login")
        .exit(4)
        .read_stdin();
    let provider = github(&sandbox, gh.exe().into(), &server.uri());
    let root = repo.path().to_owned();
    tokio::task::spawn_blocking(move || {
        let issue = provider
            .create_issue("-dash-title", "rest raw", &root)
            .unwrap();
        assert_eq!(issue.source_id, "https://github.com/owner/repo/issues/73");
        let fetched = provider.fetch_issue("owner/repo#73", &root).unwrap();
        assert_eq!(fetched.id, "73");
        assert_eq!(
            provider.update_issue(&issue, "rest raw").unwrap().body,
            "rest raw"
        );
    })
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap(),
        serde_json::json!({"title":"-dash-title","body":"rest raw"})
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[2].body).unwrap(),
        serde_json::json!({"body":"rest raw"})
    );
    server.verify().await;
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_uncertain_cli_create_never_falls_back() {
    let mut sandbox =
        support::sandbox("engine::issue::github_uncertain_cli_create_never_falls_back");
    sandbox.env_mut().set("GITHUB_TOKEN", "test-secret-github");
    sandbox.redact("test-secret-github");
    let server = server(&sandbox).await;
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("README.md", "fixture")
        .init();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue", "create"])
        .read_stdin()
        .stderr("connection lost after upload")
        .exit(1);
    let provider = github(&sandbox, gh.exe().into(), &server.uri());
    let root = repo.path().to_owned();
    tokio::task::spawn_blocking(move || {
        assert!(matches!(
            provider.create_issue("title", "body", &root),
            Err(IssueSourceError::CreateOutcomeUnknown { .. })
        ));
    })
    .await
    .unwrap();
    assert_eq!(gh.calls().len(), 1);
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_classified_cli_create_failure_never_creates_over_rest() {
    let mut sandbox = support::sandbox(
        "engine::issue::github_classified_cli_create_failure_never_creates_over_rest",
    );
    sandbox.env_mut().set("GITHUB_TOKEN", "test-secret-github");
    sandbox.redact("test-secret-github");
    let server = server(&sandbox).await;
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("README.md", "fixture")
        .init();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    // gh may have created the issue before a later step failed with this.
    gh.on(&["issue", "create"])
        .read_stdin()
        .stderr("HTTP 403: permission denied adding labels")
        .exit(1);
    let provider = github(&sandbox, gh.exe().into(), &server.uri());
    let root = repo.path().to_owned();
    tokio::task::spawn_blocking(move || {
        assert!(matches!(
            provider.create_issue("title", "body", &root),
            Err(IssueSourceError::Forbidden { .. })
        ));
    })
    .await
    .unwrap();
    assert_eq!(gh.calls().len(), 1);
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_missing_auth_and_write_permission_are_distinct() {
    let sandbox =
        support::sandbox("engine::issue::github_missing_auth_and_write_permission_are_distinct");
    let server = server(&sandbox).await;
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue", "view"]).stderr("gh auth login").exit(4);
    gh.on(&["issue", "edit"])
        .read_stdin()
        .stderr("HTTP 403: permission denied")
        .exit(1);
    let provider = github(&sandbox, gh.exe().into(), &server.uri());
    tokio::task::spawn_blocking(move || {
        assert!(matches!(provider.fetch_issue("owner/repo#73",Path::new("")), Err(IssueSourceError::Unauthorized { hint, .. }) if hint.contains("gh auth login")));
        let issue = Issue { source_id:"https://github.com/owner/repo/issues/73".into(), id:"73".into(), number:Some(73),title:"title".into(),body:"body".into(),provider:"github".into() };
        assert!(matches!(provider.update_issue(&issue,"new"), Err(IssueSourceError::Forbidden { .. })));
    }).await.unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_cli_timeout_has_a_typed_operation_and_reaps_the_child() {
    let sandbox = support::sandbox(
        "engine::issue::github_cli_timeout_has_a_typed_operation_and_reaps_the_child",
    );
    let server = server(&sandbox).await;
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue", "view"]).hold_until_released();
    let provider = github(&sandbox, gh.exe().into(), &server.uri());
    tokio::task::spawn_blocking(move || {
        assert!(matches!(
            provider.fetch_issue("owner/repo#73", Path::new("")),
            Err(IssueSourceError::Timeout {
                operation: IssueOperation::Fetch,
                ..
            })
        ));
    })
    .await
    .unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_create_timeout_is_unknown_and_never_retried() {
    let mut sandbox =
        support::sandbox("engine::issue::github_create_timeout_is_unknown_and_never_retried");
    sandbox.env_mut().set("GITHUB_TOKEN", "test-secret-github");
    sandbox.redact("test-secret-github");
    let server = server(&sandbox).await;
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("README.md", "fixture")
        .init();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue", "create"])
        .read_stdin()
        .hold_until_released();
    let provider = github(&sandbox, gh.exe().into(), &server.uri());
    let root = repo.path().to_owned();
    tokio::task::spawn_blocking(move || {
        assert!(matches!(
            provider.create_issue("title", "body", &root),
            Err(IssueSourceError::CreateOutcomeUnknown { .. })
        ));
    })
    .await
    .unwrap();
    assert_eq!(gh.calls().len(), 1);
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(server);
    sandbox.finish();
}
