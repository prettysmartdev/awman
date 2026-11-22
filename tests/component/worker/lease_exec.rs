//! WI 0162 over the wire: the optional `lease_exec` conformance probes against
//! the mock work server, and the worker's fail-closed admission of lease exec
//! fields it was never registered for or cannot decode. Full pulled runs are
//! `daemon::lease_exec`.
use crate::support;
use awman::command::worker::policy::{self, LeaseHostView, SkillStatus};
use awman::data::config::{
    daemon::{Interaction, WorkerSettings},
    work_servers::WorkServerConfig,
};
use awman::engine::worker::lease_workspace::LeaseRuntimeSupport;
use awman::engine::worker::{LeaseRef, WspSession};
use awman_test_support::fakes::FakeWorkServer;
use awman_wsp::mock::{MockFault, MockWorkServer};
use awman_wsp::{
    conformance::lease_exec, features, secret::SecretString, ClaimOutcome, ExecCredentialChoice,
    ExecPrerequisites, ExecSecrets, ExecWorkflowsDynamic, RegisterRequest, ReleaseReason,
    ReleaseRequest, WorkItem, WorkKind,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

const VALUE: &str = "test-secret-worker-lease-exec";

async fn linked(server: &FakeWorkServer, name: &str, features: &[&str]) -> Arc<WspSession> {
    let link = server.server().link();
    server.server().approve_link(&link.link_id, BTreeMap::new());
    let client = awman_wsp::Client::new(server.base_url(), String::new()).unwrap();
    let token = client.poll_link(&link.link_id).await.unwrap().unwrap();
    let session =
        Arc::new(WspSession::connect(name, server.base_url(), token.refresh_token).unwrap());
    session
        .register(&RegisterRequest {
            name: name.into(),
            labels: BTreeMap::new(),
            capacity: 8,
            features: features.iter().map(|f| f.to_string()).collect(),
        })
        .await
        .unwrap();
    session
}

fn settings() -> WorkerSettings {
    WorkerSettings {
        enabled: true,
        max_concurrent_jobs: 1,
        push_enabled: false,
        push_interaction: Interaction::Headless,
        push_allow_override: false,
        push_work_dirs: vec![],
        push_work_dir_roots: vec![],
        pull_enabled: true,
        pull_interaction: Interaction::Headless,
        pull_allow_override: false,
        pull_work_root: None,
        work_servers: Default::default(),
    }
}

/// Production admission on a worker registered with `registered`: an
/// installed, allowed `planner` skill, an allowed and set worker variable,
/// and a worker policy that grants nothing.
fn admit(item: &WorkItem, registered: &[&str]) -> Result<policy::LeasePlan, String> {
    let mut server = WorkServerConfig::default().resolved();
    server.allow_env = vec!["WORKER_VAR".into()];
    server.allow_skills = vec!["planner".into()];
    let prerequisites = awman::data::config::workflows::PrerequisitePolicy::default();
    policy::plan(
        item,
        &BTreeMap::new(),
        &server,
        &settings(),
        &LeaseRuntimeSupport {
            read_only_workspace: true,
            outputs_mount: true,
            skill_mounts: true,
            secret_env: true,
        },
        "docker",
        &LeaseHostView {
            features: registered,
            worker_env: &|name| (name == "WORKER_VAR").then(|| "set".to_string()),
            skills: &|name| {
                if name == "planner" {
                    SkillStatus::Installed {
                        default_library: false,
                    }
                } else {
                    SkillStatus::Missing
                }
            },
            prerequisite_policy: Some(&prerequisites),
        },
    )
}

/// One item per exec feature, each needing exactly that feature.
fn items() -> Vec<(WorkItem, &'static str)> {
    let base = |id: &str| awman_wsp::conformance::work_item(id);
    let mut secret = base("uses-secrets");
    secret.exec.secrets = ExecSecrets::new([("TOKEN".into(), SecretString::new(VALUE))]).unwrap();
    let mut env = base("uses-env");
    env.exec.overlays = vec!["env(WORKER_VAR)".into()];
    let mut skill = base("uses-skill");
    skill.exec.overlays = vec!["skill(planner)".into()];
    let mut prerequisites = base("uses-prerequisites");
    prerequisites.kind = WorkKind::Workflow;
    prerequisites.exec.workflow_toml = Some("[[steps]]\nname = \"a\"\nprompt = \"x\"\n".into());
    prerequisites.exec.prerequisites = Some(ExecPrerequisites {
        git_identity: None,
        ssh: Some(ExecCredentialChoice::Deny),
        github_token: None,
    });
    let mut dynamic = base("uses-dynamic");
    dynamic.kind = WorkKind::Workflow;
    dynamic.exec.dynamic = true;
    dynamic.exec.workflows_dynamic = Some(ExecWorkflowsDynamic {
        agents_to_models: None,
        default_leader: Some("claude::model-x".into()),
        guidance: None,
    });
    vec![
        (secret, features::LEASE_SECRETS),
        (env, features::LEASE_SECRETS),
        (skill, features::LEASE_SKILLS),
        (prerequisites, features::LEASE_PREREQUISITES),
        (dynamic, features::LEASE_WORKFLOWS_DYNAMIC),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_probes_pass_against_the_mock_and_detect_a_server_ignoring_exec_features() {
    let sandbox = support::sandbox(
        "worker::lease_exec::conformance_probes_pass_against_the_mock_and_detect_a_server_ignoring_exec_features",
    );
    sandbox.redact("wire-secret");
    let server = FakeWorkServer::start(&sandbox).await;
    // Each new field round-trips, is offered only to a capable worker, an
    // incapable worker's claim is released `ineligible`, and a refused link
    // answers 403 `access_denied`.
    let report = lease_exec::run(server.base_url(), server.server().as_ref()).await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.passed, lease_exec::cases());
    for case in [
        "exec_secrets_round_trip",
        "exec_prerequisites_round_trip",
        "exec_workflows_dynamic_round_trip",
        "exec_overlays_round_trip",
        "incapable_worker_releases_ineligible",
        "link_refused_is_403",
    ] {
        assert!(report.passed.contains(&case), "{case} is not probed");
    }
    server.shutdown().await;
    // A server that offers exec fields to any worker fails the offer probe.
    let faulty = FakeWorkServer::start_with(
        &sandbox,
        Arc::new(MockWorkServer::with_fault(
            MockFault::IgnoreExecCapabilities,
        )),
    )
    .await;
    assert!(lease_exec::run_case(
        faulty.base_url(),
        faulty.server().as_ref(),
        "exec_fields_offered_only_to_capable_workers"
    )
    .await
    .is_err());
    // Offered anyway, the probe's worker releases `ineligible`; the server
    // must not re-offer it to that worker and must still deliver it.
    lease_exec::run_case(
        faulty.base_url(),
        faulty.server().as_ref(),
        "incapable_worker_releases_ineligible",
    )
    .await
    .unwrap();
    faulty.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_without_a_feature_is_never_offered_it_and_releases_a_forced_offer_ineligible() {
    let sandbox = support::sandbox(
        "worker::lease_exec::a_worker_without_a_feature_is_never_offered_it_and_releases_a_forced_offer_ineligible",
    );
    sandbox.redact(VALUE);
    // A conforming server never offers the fields to a worker without them.
    let server = FakeWorkServer::start(&sandbox).await;
    let plain = linked(&server, "plain", &[]).await;
    for (item, _) in items() {
        server.server().enqueue(item, chrono::Utc::now()).unwrap();
    }
    assert!(matches!(
        plain.claim(None, 0).await.unwrap(),
        ClaimOutcome::Empty
    ));
    // A capable worker is offered each one, and admits it.
    let capable: Vec<&str> = features::ALL.to_vec();
    let worker = linked(&server, "capable", &capable).await;
    for (item, feature) in items() {
        let ClaimOutcome::Leased(lease) = worker.claim(Some(&item.id), 0).await.unwrap() else {
            panic!("{} was not offered", item.id);
        };
        admit(&lease.item, &capable).unwrap_or_else(|e| panic!("{feature}: {e}"));
        worker
            .release(
                &LeaseRef::of(&lease),
                &ReleaseRequest::<awman::data::fs::api_db::CommandResult> {
                    context_changes: None,
                    reason: ReleaseReason::UserDeclined,
                    message: None,
                    result: None,
                },
            )
            .await
            .unwrap();
    }
    server.shutdown().await;

    // A server that ignores registered features hands the worker each item
    // anyway: admission refuses it naming the missing feature, and the worker
    // releases `ineligible` (the reference worker's rule).
    let faulty = FakeWorkServer::start_with(
        &sandbox,
        Arc::new(MockWorkServer::with_fault(
            MockFault::IgnoreExecCapabilities,
        )),
    )
    .await;
    let plain = linked(&faulty, "plain", &[]).await;
    for (item, feature) in items() {
        let id = item.id.clone();
        faulty.server().enqueue(item, chrono::Utc::now()).unwrap();
        let ClaimOutcome::Leased(lease) = plain.claim(Some(&id), 0).await.unwrap() else {
            panic!("{id} was not handed out");
        };
        let refusal = admit(&lease.item, &[]).expect_err(&id);
        assert_eq!(refusal, format!("this worker does not support {feature}"));
        // Every other feature registered still does not admit it.
        let others: Vec<&str> = capable.iter().copied().filter(|f| *f != feature).collect();
        assert!(admit(&lease.item, &others).is_err(), "{id}");
        plain
            .release(
                &LeaseRef::of(&lease),
                &ReleaseRequest::<awman::data::fs::api_db::CommandResult> {
                    context_changes: None,
                    reason: ReleaseReason::Ineligible,
                    message: Some(refusal),
                    result: None,
                },
            )
            .await
            .unwrap();
        let release = faulty.server().release_for(&lease.id).expect("released");
        assert_eq!(release.reason, ReleaseReason::Ineligible);
        assert!(!serde_json::to_string(&release).unwrap().contains(VALUE));
    }
    faulty.shutdown().await;
    sandbox.finish();
}

/// A server sends a lease whose `exec` this worker cannot represent: the
/// client releases it `ineligible` and never returns a weaker lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_or_invalid_exec_fields_from_a_server_fail_closed() {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let sandbox = support::sandbox(
        "worker::lease_exec::unknown_or_invalid_exec_fields_from_a_server_fail_closed",
    );
    sandbox.redact(VALUE);
    let mut item = awman_wsp::conformance::work_item("strict");
    item.exec.secrets = ExecSecrets::new([("TOKEN".into(), SecretString::new(VALUE))]).unwrap();
    let lease = awman_wsp::Lease {
        id: "lease-strict".into(),
        epoch: 7,
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        item,
        terminal: false,
        remote_input: false,
    };
    let good = awman_wsp::secret::server_wire::lease(&lease);
    type Edit = (&'static str, fn(&mut Value));
    let edits: Vec<Edit> = vec![
        ("exec field", |l| l["item"]["exec"]["surprise"] = json!(1)),
        ("prerequisites field", |l| {
            l["item"]["exec"]["prerequisites"] = json!({"ssh":"deny","surprise":true})
        }),
        ("prerequisites choice", |l| {
            l["item"]["exec"]["prerequisites"] = json!({"ssh":"maybe"})
        }),
        ("workflows_dynamic field", |l| {
            l["item"]["exec"]["workflows_dynamic"] = json!({"surprise":true})
        }),
        ("secret name", |l| {
            l["item"]["exec"]["secrets"] = json!({"lower": "x"})
        }),
        ("secret value", |l| {
            l["item"]["exec"]["secrets"] = json!({"TOKEN": 7})
        }),
    ];
    for (label, edit) in edits {
        let server = MockServer::builder()
            .listener(sandbox.ports().tcp_listener())
            .start()
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"t"})))
            .mount(&server)
            .await;
        let mut body = good.clone();
        edit(&mut body);
        Mock::given(method("POST"))
            .and(path("/v1/claims"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "lease": body })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex("^/v1/leases/lease-strict/release$"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = awman_wsp::Client::new(&server.uri(), "refresh".into()).unwrap();
        let error = client.claim(Some("strict"), 0).await.expect_err(label);
        assert!(
            matches!(&error, awman_wsp::Error::Conflict { code, .. } if code == awman_wsp::codes::INELIGIBLE),
            "{label}: {error}"
        );
        assert!(!error.to_string().contains(VALUE));
        let release: Value = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path().ends_with("/release"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("released");
        assert_eq!(release["reason"], "ineligible", "{label}");
        drop(server);
    }
    // Control: the unmodified lease decodes, secrets included.
    let server = MockServer::builder()
        .listener(sandbox.ports().tcp_listener())
        .start()
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"t"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/claims"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "lease": good })))
        .mount(&server)
        .await;
    let client = awman_wsp::Client::new(&server.uri(), "refresh".into()).unwrap();
    let decoded = client.claim(Some("strict"), 0).await.unwrap().unwrap();
    assert_eq!(decoded.item.exec.secrets.names(), ["TOKEN"]);
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_secret_keys_fields_and_excess_entries_are_refused_over_http() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let sandbox = support::sandbox(
        "worker::lease_exec::duplicate_secret_keys_fields_and_excess_entries_are_refused_over_http",
    );
    sandbox.redact(VALUE);
    let lease = awman_wsp::Lease {
        id: "duplicate-lease".into(),
        epoch: 19,
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        item: awman_wsp::conformance::work_item("duplicate"),
        terminal: false,
        remote_input: false,
    };
    let plain = serde_json::to_string(&json!({"lease":lease})).unwrap();
    let too_many = (0..33)
        .map(|i| format!("\"TOKEN_{i}\":\"{VALUE}\""))
        .collect::<Vec<_>>()
        .join(",");
    for fields in [
        format!("\"secrets\":{{\"TOKEN\":\"first-private-value\",\"TOKEN\":\"{VALUE}\"}},"),
        format!("\"secrets\":{{\"FIRST\":\"first-private-value\"}},\"secrets\":{{\"TOKEN\":\"{VALUE}\"}},"),
        format!("\"secrets\":{{{too_many}}},"),
    ] {
        let server = MockServer::builder().listener(sandbox.ports().tcp_listener()).start().await;
        Mock::given(method("POST")).and(path("/v1/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"t"})))
            .mount(&server).await;
        // Keep duplicate JSON keys intact all the way through HTTP transport.
        let body = plain.replacen("\"exec\":{", &format!("\"exec\":{{{fields}"), 1);
        assert_ne!(body, plain);
        Mock::given(method("POST")).and(path("/v1/claims"))
            .respond_with(ResponseTemplate::new(201).set_body_raw(body, "application/json"))
            .mount(&server).await;
        Mock::given(method("POST")).and(path("/v1/leases/duplicate-lease/release"))
            .and(header("x-wsp-epoch", "19"))
            .respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
        let client = awman_wsp::Client::new(&server.uri(), "refresh".into()).unwrap();
        let error = client.claim(None, 0).await.expect_err("invalid secret object");
        assert!(matches!(&error, awman_wsp::Error::Conflict { code, .. } if code == awman_wsp::codes::INELIGIBLE));
        let requests = server.received_requests().await.unwrap();
        let release = requests.iter().find(|r| r.url.path().ends_with("/release")).expect("fenced release");
        let diagnostic = format!("{error:?} {error} {}", String::from_utf8_lossy(&release.body));
        assert!(!diagnostic.contains(VALUE) && !diagnostic.contains("first-private-value"));
        assert_eq!(serde_json::from_slice::<Value>(&release.body).unwrap()["reason"], "ineligible");
    }
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toml_only_overlays_require_features_in_mock_offers_and_reference_workers() {
    use awman_wsp::mock_worker::{MockOutcome, MockWorker};
    let sandbox = support::sandbox("worker::lease_exec::toml_only_overlays_require_features_in_mock_offers_and_reference_workers");
    let conforming = FakeWorkServer::start(&sandbox).await;
    let faulty = FakeWorkServer::start_with(
        &sandbox,
        Arc::new(MockWorkServer::with_fault(
            MockFault::IgnoreExecCapabilities,
        )),
    )
    .await;
    let plain = linked(&conforming, "featureless", &[]).await;
    // Inspect both representations and both capability names through real
    // mock HTTP offers and a forced offer to the reference worker.
    for (i, overlay) in ["env(WORKER_VAR)", "skill(planner)"].iter().enumerate() {
        for top_level in [true, false] {
            let mut item = awman_wsp::conformance::work_item(&format!("toml-{i}-{top_level}"));
            item.kind = WorkKind::Workflow;
            let step = "[[steps]]\nname = \"a\"\nprompt = \"x\"\n";
            item.exec.workflow_toml = Some(if top_level {
                format!("overlays = [{overlay:?}]\n{step}")
            } else {
                format!("{step}overlays = [{overlay:?}]\n")
            });
            conforming
                .server()
                .enqueue(item.clone(), chrono::Utc::now())
                .unwrap();
            assert!(matches!(
                plain.claim(None, 0).await.unwrap(),
                ClaimOutcome::Empty
            ));
            faulty.server().enqueue(item, chrono::Utc::now()).unwrap();
        }
    }
    let link = faulty.server().link();
    faulty.server().approve_link(&link.link_id, BTreeMap::new());
    let client = awman_wsp::Client::new(faulty.base_url(), String::new()).unwrap();
    let token = client.poll_link(&link.link_id).await.unwrap().unwrap();
    let client = awman_wsp::Client::new(faulty.base_url(), token.refresh_token).unwrap();
    let labels = awman_wsp::labels::WorkerLabels::new(BTreeMap::new(), BTreeMap::new()).unwrap();
    let mut worker = MockWorker::new(client, "reference", labels, vec![]);
    for _ in 0..4 {
        let report = worker
            .run_once(MockOutcome::Succeed {
                result: json!({}),
                events: vec![],
            })
            .await
            .unwrap()
            .expect("forced offer");
        assert!(matches!(
            report.outcome,
            MockOutcome::Release(ReleaseReason::Ineligible)
        ));
        assert_eq!(
            faulty
                .server()
                .release_for(&report.lease.id)
                .unwrap()
                .reason,
            ReleaseReason::Ineligible
        );
    }
    conforming.shutdown().await;
    faulty.shutdown().await;
    sandbox.finish();
}
