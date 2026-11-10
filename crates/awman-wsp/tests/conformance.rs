use awman_wsp::{
    conformance::work_item,
    labels::WorkerLabels,
    mock::{MockWorkServer, ServingMock},
    *,
};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Instant};

async fn setup() -> (Arc<MockWorkServer>, ServingMock, Client) {
    let server = Arc::new(MockWorkServer::new());
    let serving = server.clone().serve().await.unwrap();
    let client = link(&server, &serving.base_url, "first", BTreeMap::new()).await;
    (server, serving, client)
}
async fn link(
    server: &MockWorkServer,
    url: &str,
    name: &str,
    server_labels: BTreeMap<String, String>,
) -> Client {
    let linker = Client::new(url, String::new()).unwrap();
    let start = linker.start_link().await.unwrap();
    assert!(linker.poll_link(&start.link_id).await.unwrap().is_none());
    server.approve_link(&start.link_id, server_labels);
    let token = linker.poll_link(&start.link_id).await.unwrap().unwrap();
    let client = Client::new(url, token.refresh_token).unwrap();
    client
        .register(&RegisterRequest {
            name: name.into(),
            labels: BTreeMap::from([("pool".into(), "test".into())]),
            capacity: 10,
            features: features::ALL.iter().map(|f| f.to_string()).collect(),
        })
        .await
        .unwrap();
    client
}
fn heartbeat() -> HeartbeatRequest {
    HeartbeatRequest {
        state: HeartbeatState::Running,
        step: Some("step".into()),
        progress: Some(json!({"percent":25})),
    }
}
fn result() -> LeaseResult<Value> {
    LeaseResult {
        result: json!({"exit_code":0,"outcome":"done","git":{"pushed":{"sha":"abc"}},"final_message":"ok"}),
        outputs: Some(BTreeMap::from([("file.txt".into(), "aGk=".into())])),
        outputs_mounted: true,
        duration_ms: 123,
    }
}
async fn claimed(s: &MockWorkServer, c: &Client, id: &str) -> Lease {
    s.enqueue(work_item(id), Utc::now()).unwrap();
    c.claim(Some(id), 0).await.unwrap().unwrap()
}
#[tokio::test]
async fn reusable_conformance_suite_passes_over_http() {
    let server = Arc::new(MockWorkServer::new());
    let serving = server.clone().serve().await.unwrap();
    let report = conformance::run(&serving.base_url, server.as_ref()).await;
    assert!(report.is_success(), "{report:?}");
    assert_eq!(report.passed.len(), conformance::cases().len());
}
#[tokio::test]
async fn link_device_flow_then_register() {
    let (s, http, c) = setup().await;
    let r = c
        .register(&RegisterRequest {
            name: "changed".into(),
            labels: BTreeMap::new(),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap();
    let r2 = c
        .register(&RegisterRequest {
            name: "renamed".into(),
            labels: BTreeMap::new(),
            capacity: 2,
            features: vec![],
        })
        .await
        .unwrap();
    assert_eq!(r.worker_id, r2.worker_id);
    assert!(r.heartbeat_ms > 0 && r.poll_ms > 0);
    let linker = Client::new(&http.base_url, String::new()).unwrap();
    let denied = linker.start_link().await.unwrap();
    s.deny_link(&denied.link_id);
    assert!(matches!(
        linker.poll_link(&denied.link_id).await,
        Err(Error::LinkDenied)
    ));
    let expired = linker.start_link().await.unwrap();
    s.expire_link(&expired.link_id);
    let e = linker.poll_link(&expired.link_id).await.unwrap_err();
    assert_eq!(e.code(), Some("expired"));
}
#[tokio::test]
async fn worker_cannot_set_server_assigned_labels() {
    let (s, http, c) = setup().await;
    let _trusted = link(
        &s,
        &http.base_url,
        "trusted",
        BTreeMap::from([("owner".into(), "alice".into())]),
    )
    .await;
    let e = c
        .register(&RegisterRequest {
            name: "spoof".into(),
            labels: BTreeMap::from([("owner".into(), "alice".into())]),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some("server_label_refused"));
    assert!(WorkerLabels::new(
        BTreeMap::new(),
        BTreeMap::from([("awman/os".into(), "spoof".into())])
    )
    .is_err());
    let mut item = work_item("trusted");
    item.affinity.required = vec![Match {
        key: "owner".into(),
        op: MatchOp::In,
        values: vec!["alice".into()],
    }];
    s.enqueue(item, Utc::now()).unwrap();
    assert!(c.queue(50, None).await.unwrap().items.is_empty());
}
#[tokio::test]
async fn affinity_all_operators_filter_and_preferred_weight_orders_ties() {
    let (s, _http, c) = setup().await;
    let now = Utc::now();
    let cases = [
        ("in", "pool", MatchOp::In, vec!["test"], true),
        ("in-no", "pool", MatchOp::In, vec!["other"], false),
        ("not-in", "pool", MatchOp::NotIn, vec!["other"], true),
        ("not-in-no", "pool", MatchOp::NotIn, vec!["test"], false),
        (
            "not-in-missing",
            "missing",
            MatchOp::NotIn,
            vec!["test"],
            true,
        ),
        ("exists", "pool", MatchOp::Exists, vec![], true),
        ("exists-no", "missing", MatchOp::Exists, vec![], false),
        ("absent", "missing", MatchOp::DoesNotExist, vec![], true),
        ("absent-no", "pool", MatchOp::DoesNotExist, vec![], false),
    ];
    for (id, key, op, values, _) in &cases {
        let mut i = work_item(id);
        i.affinity.required = vec![Match {
            key: (*key).into(),
            op: *op,
            values: values.iter().map(|v| v.to_string()).collect(),
        }];
        s.enqueue(i, now).unwrap();
    }
    let queue = c.queue(50, None).await.unwrap();
    for (id, _, _, _, expected) in cases {
        assert_eq!(queue.items.iter().any(|i| i.id == id), expected, "{id}");
    }
    let mut invalid = work_item("invalid");
    invalid.affinity.preferred = vec![Preferred {
        weight: 0,
        matches: vec![],
    }];
    assert_eq!(
        s.enqueue(invalid, now).unwrap_err().code,
        "invalid_affinity"
    );
    for (id, priority, age, weight) in [
        ("prio", 5, 0, 0),
        ("older", 4, -10, 0),
        ("prefer", 4, 0, 100),
        ("plain", 4, 0, 0),
    ] {
        let mut i = work_item(id);
        i.priority = priority;
        if weight > 0 {
            i.affinity.preferred = vec![Preferred {
                weight,
                matches: vec![Match {
                    key: "pool".into(),
                    op: MatchOp::Exists,
                    values: vec![],
                }],
            }];
        }
        s.enqueue(i, now + Duration::seconds(age)).unwrap();
    }
    let queue = c.queue(4, None).await.unwrap();
    assert_eq!(
        queue
            .items
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["prio", "older", "prefer", "plain"]
    );
    let next = c.queue(50, queue.next_cursor.as_deref()).await.unwrap();
    assert!(next.items.iter().all(|i| i.priority == 0));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_claims_on_same_item_yield_one_lease_and_409_taken() {
    let (s, http, a) = setup().await;
    let b = link(&s, &http.base_url, "second", BTreeMap::new()).await;
    s.enqueue(work_item("race"), Utc::now()).unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let tasks: Vec<_> = [a, b]
        .into_iter()
        .map(|c| {
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                c.claim(Some("race"), 0).await
            })
        })
        .collect();
    let mut wins = 0;
    let mut taken = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(Some(_)) => wins += 1,
            Err(Error::Conflict { code, .. }) if code == "taken" => taken += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((wins, taken), (1, 1));
}
#[tokio::test]
async fn missed_heartbeat_requeues_item_and_old_lease_gets_409_lease_lost() {
    let (s, _http, c) = setup().await;
    let lease = claimed(&s, &c, "expiry").await;
    let before = lease.expires_at;
    let h = c.heartbeat(&lease.id, &heartbeat()).await.unwrap();
    assert!(h.expires_at >= before);
    // Exercise actual expiry, using the deterministic clock on the in-process state.
    assert_eq!(
        s.queue_at(
            &s.worker_id_for_lease(&lease.id).unwrap(),
            h.expires_at + Duration::milliseconds(1),
            50,
            None
        )
        .unwrap()
        .items
        .len(),
        1
    );
    assert!(c
        .heartbeat(&lease.id, &heartbeat())
        .await
        .unwrap_err()
        .is_lease_lost());
    assert!(c
        .result(&lease.id, &result())
        .await
        .unwrap_err()
        .is_lease_lost());
    assert!(c
        .events(&lease.id, vec![json!({"attempt":1,"sequence":0})])
        .await
        .unwrap_err()
        .is_lease_lost());
    assert!(c.credentials(&lease.id).await.unwrap_err().is_lease_lost());
    assert!(c
        .release(
            &lease.id,
            &ReleaseRequest::<Value> {
                reason: ReleaseReason::Shutdown,
                message: None,
                result: None
            }
        )
        .await
        .unwrap_err()
        .is_lease_lost());
    let next = c.claim(Some("expiry"), 0).await.unwrap().unwrap();
    assert!(next.epoch > lease.epoch);
    assert_ne!(next.id, lease.id);
    assert_eq!(
        s.heartbeat(
            &next.id,
            lease.epoch,
            Utc::now(),
            Duration::seconds(3),
            &heartbeat()
        )
        .unwrap_err()
        .code,
        "lease_lost"
    );
}
#[tokio::test]
async fn events_duplicate_attempt_sequence_are_accepted_once() {
    let (s, _http, c) = setup().await;
    let l = claimed(&s, &c, "events").await;
    let e = json!({"attempt":1,"sequence":2,"type":"workflow_state","data":{"phases":[]}});
    c.events(&l.id, vec![e.clone(), e.clone()]).await.unwrap();
    c.events(&l.id, vec![e.clone()]).await.unwrap();
    assert_eq!(s.event_count(&l.id), Some(1));
    let mut later = e.clone();
    later["attempt"] = json!(2);
    c.events(&l.id, vec![later.clone()]).await.unwrap();
    assert_eq!(s.events_for(&l.id), vec![e, later]);
    let bad = json!({"attempt":u64::from(u32::MAX)+1,"sequence":1});
    assert_eq!(
        c.events(&l.id, vec![bad]).await.unwrap_err().code(),
        Some("invalid_event")
    );
    let valid = json!({"attempt":3,"sequence":1});
    assert!(c.events(&l.id, vec![valid, json!({})]).await.is_err());
    assert_eq!(s.event_count(&l.id), Some(2));
}
#[tokio::test]
async fn result_embeds_command_result_unchanged_and_ends_lease() {
    let (s, _http, c) = setup().await;
    let l = claimed(&s, &c, "result").await;
    c.result(&l.id, &result()).await.unwrap();
    assert_eq!(s.result_for("result").unwrap(), result());
    assert!(c
        .result(&l.id, &result())
        .await
        .unwrap_err()
        .is_lease_lost());
    assert!(c.queue(50, None).await.unwrap().items.is_empty());
}
#[tokio::test]
async fn claim_long_poll_returns_201_when_item_arrives_and_204_on_timeout() {
    let (s, _http, c) = setup().await;
    let now = Instant::now();
    assert_eq!(
        c.claim_outcome(None, 70).await.unwrap(),
        ClaimOutcome::Empty
    );
    assert!(now.elapsed() >= std::time::Duration::from_millis(60));
    let worker = c.clone();
    let waiting = tokio::spawn(async move { worker.claim_outcome(None, 2000).await });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    s.enqueue(work_item("arrived"), Utc::now()).unwrap();
    assert!(
        matches!(waiting.await.unwrap().unwrap(),ClaimOutcome::Leased(l) if l.item.id=="arrived")
    );
}
#[tokio::test]
async fn release_unrun_item_requeues_and_release_midrun_attaches_partial_result() {
    let (s, http, c) = setup().await;
    let b = link(&s, &http.base_url, "second", BTreeMap::new()).await;
    for (index, reason) in [
        ReleaseReason::UserDeclined,
        ReleaseReason::Shutdown,
        ReleaseReason::Error,
        ReleaseReason::Ineligible,
    ]
    .into_iter()
    .enumerate()
    {
        let l = claimed(&s, &c, &format!("release{index}")).await;
        let req = ReleaseRequest {
            reason,
            message: Some("partial run".into()),
            result: Some(json!({"outcome":"cancelled","exit_code":null})),
        };
        c.release(&l.id, &req).await.unwrap();
        assert_eq!(s.release_for(&l.id).unwrap(), req);
        if reason == ReleaseReason::Ineligible {
            assert!(c
                .claim(Some(&l.item.id), 0)
                .await
                .unwrap_err()
                .is_ineligible());
        }
        let next = b.claim(Some(&l.item.id), 0).await.unwrap().unwrap();
        b.result(&next.id, &result()).await.unwrap();
    }
}
#[tokio::test]
async fn credentials_endpoint_returns_fresh_repo_credentials_for_live_lease_only() {
    let (s, _http, c) = setup().await;
    let l = claimed(&s, &c, "creds").await;
    let creds = Credentials {
        kind: CredentialKind::Basic,
        token: Some("private-token".into()),
        username: Some("private-user".into()),
        expires_at: Some(crate_time()),
    };
    s.set_credentials("creds", creds.clone());
    assert_eq!(c.credentials(&l.id).await.unwrap(), creds);
    c.result(&l.id, &result()).await.unwrap();
    assert!(c.credentials(&l.id).await.unwrap_err().is_lease_lost());
}
fn crate_time() -> chrono::DateTime<Utc> {
    wire::now() + Duration::hours(1)
}
#[tokio::test]
async fn expired_access_token_is_refreshed_once_and_request_retried() {
    let (s, _http, c) = setup().await;
    s.invalidate_access_tokens();
    assert!(c.queue(50, None).await.is_ok());
    let revoked = s.link();
    s.approve_link(&revoked.link_id, BTreeMap::new());
    // Revoking a cached link causes a 401 followed by a refused refresh.
    let refresh = {
        let state = Client::new(&_http.base_url, String::new()).unwrap();
        state
            .poll_link(&revoked.link_id)
            .await
            .unwrap()
            .unwrap()
            .refresh_token
    };
    let client = Client::new(&_http.base_url, refresh.clone()).unwrap();
    client
        .register(&RegisterRequest {
            name: "revoked".into(),
            labels: BTreeMap::new(),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap();
    s.revoke(&refresh);
    assert!(matches!(
        client.queue(50, None).await,
        Err(Error::LinkRevoked)
    ));
}
#[tokio::test]
async fn every_request_requires_version_and_bearer_and_fences_other_workers() {
    let (s, http, a) = setup().await;
    let b = link(&s, &http.base_url, "second", BTreeMap::new()).await;
    let l = claimed(&s, &a, "owned").await;
    assert!(b
        .heartbeat(&l.id, &heartbeat())
        .await
        .unwrap_err()
        .is_lease_lost());
    let client = reqwest::Client::new();
    for path in [
        "v1/link",
        "v1/token",
        "v1/workers/register",
        "v1/claims",
        "v1/leases/id/events",
        "v1/leases/id/heartbeat",
        "v1/leases/id/result",
        "v1/leases/id/release",
        "v1/leases/id/credentials",
    ] {
        let response = client
            .post(format!("{}/{path}", http.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        let error: ApiError = response.json().await.unwrap();
        assert_eq!(error.code, "unsupported_version");
    }
    let r = client
        .get(format!("{}/v1/queue", http.base_url))
        .header(VERSION_HEADER, WSP_VERSION)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
#[tokio::test]
async fn mock_worker_is_usable_against_http_server() {
    use awman_wsp::mock_worker::{MockOutcome, MockWorker};
    let (s, _http, c) = setup().await;
    s.enqueue(work_item("worker"), Utc::now()).unwrap();
    let mut worker = MockWorker::new(
        c,
        "scripted",
        WorkerLabels::default(),
        vec!["prompt".into()],
    );
    let report = worker
        .run_once(MockOutcome::Succeed {
            result: json!({"outcome":"done","exit_code":0}),
            events: vec![json!({"attempt":1,"sequence":1})],
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.lease.item.id, "worker");
    assert_eq!(s.result_for("worker").unwrap().result["outcome"], "done");
}
#[cfg(feature = "terminal")]
#[tokio::test]
async fn terminal_socket_multiplexes_frames_by_terminal_id_and_refuses_ungranted_input() {
    use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame};
    use futures_util::{SinkExt, StreamExt};
    let (s, _http, c) = setup().await;
    for grant in [false, true] {
        let id = format!("term{grant}");
        s.enqueue(work_item(&id), Utc::now()).unwrap();
        s.set_terminal(&id, true, grant);
        let l = c.claim(Some(&id), 0).await.unwrap().unwrap();
        assert!(l.terminal);
        assert_eq!(l.remote_input, grant);
        let mut socket = c.open_terminal(&l.id).await.unwrap();
        for terminal in ["term_a", "term_b"] {
            socket
                .send(LeaseTerminalFrame {
                    terminal_id: terminal.into(),
                    frame: TerminalFrame::Output {
                        bytes: "aGk=".into(),
                    },
                })
                .await
                .unwrap();
        }
        let input = LeaseTerminalFrame {
            terminal_id: "term_b".into(),
            frame: TerminalFrame::Input {
                bytes: "eA==".into(),
            },
        };
        if grant {
            s.send_terminal(&l.id, input.clone()).unwrap();
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                input
            );
        } else {
            assert_eq!(
                s.send_terminal(&l.id, input).unwrap_err().code,
                "remote_input_refused"
            );
        }
        for _ in 0..50 {
            if s.terminal_frames(&l.id).len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            s.terminal_frames(&l.id)
                .iter()
                .map(|f| f.terminal_id.as_str())
                .collect::<Vec<_>>(),
            ["term_a", "term_b"]
        );
        c.result(&l.id, &result()).await.unwrap();
        assert!(c.open_terminal(&l.id).await.unwrap_err().is_lease_lost());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
                .await
                .is_ok()
        );
    }
}

#[tokio::test]
async fn link_static_token_then_register_and_update_server_labels() {
    let server = Arc::new(MockWorkServer::new());
    let serving = server.clone().serve().await.unwrap();
    let link = server.link();
    server.approve_link(
        &link.link_id,
        BTreeMap::from([("owner".into(), "alice".into())]),
    );
    let linker = Client::new(&serving.base_url, String::new()).unwrap();
    let refresh = linker
        .poll_link(&link.link_id)
        .await
        .unwrap()
        .unwrap()
        .refresh_token;
    let client = Client::new(&serving.base_url, refresh).unwrap();
    let request = RegisterRequest {
        name: "static".into(),
        labels: BTreeMap::new(),
        capacity: 1,
        features: vec![],
    };
    let registration = client.register(&request).await.unwrap();
    assert_eq!(registration.server_labels["owner"], "alice");
    let mut item = work_item("assigned");
    item.affinity.required = vec![Match {
        key: "owner".into(),
        op: MatchOp::In,
        values: vec!["bob".into()],
    }];
    server.enqueue(item, Utc::now()).unwrap();
    assert!(client.queue(50, None).await.unwrap().items.is_empty());
    server.approve_link(
        &link.link_id,
        BTreeMap::from([("owner".into(), "bob".into())]),
    );
    assert_eq!(client.queue(50, None).await.unwrap().items.len(), 1);
    let next = client.register(&request).await.unwrap();
    assert_eq!(registration.worker_id, next.worker_id);
    assert_eq!(next.server_labels["owner"], "bob");
}
#[tokio::test]
async fn heartbeat_preserves_request_and_injects_cancellation() {
    let (s, _http, c) = setup().await;
    let lease = claimed(&s, &c, "cancel").await;
    let request = heartbeat();
    c.heartbeat(&lease.id, &request).await.unwrap();
    assert_eq!(s.heartbeat_for(&lease.id), Some(request.clone()));
    s.request_cancel(&lease.id, "operator cancelled");
    let reply = c.heartbeat(&lease.id, &request).await.unwrap();
    assert!(reply.cancel);
    assert_eq!(reply.cancel_reason.as_deref(), Some("operator cancelled"));
}

#[tokio::test]
async fn later_server_label_ownership_fences_existing_spoofed_registrations() {
    let (s, http, attacker) = setup().await;
    attacker
        .register(&RegisterRequest {
            name: "attacker".into(),
            labels: BTreeMap::from([("team.example/owner".into(), "victim".into())]),
            capacity: 2,
            features: vec![],
        })
        .await
        .unwrap();
    let victim_link = s.link();
    s.approve_link(
        &victim_link.link_id,
        BTreeMap::from([("team.example/owner".into(), "victim".into())]),
    );
    let mut item = work_item("private-item");
    item.affinity.required.push(Match {
        key: "team.example/owner".into(),
        op: MatchOp::In,
        values: vec!["victim".into()],
    });
    s.enqueue(item, Utc::now()).unwrap();
    assert!(attacker.queue(50, None).await.unwrap().items.is_empty());
    assert_eq!(
        attacker
            .claim(Some("private-item"), 0)
            .await
            .unwrap_err()
            .code(),
        Some("ineligible")
    );
    s.approve_link(&victim_link.link_id, BTreeMap::new());
    assert!(attacker.queue(50, None).await.unwrap().items.is_empty());
    let error = attacker
        .register(&RegisterRequest {
            name: "attacker".into(),
            labels: BTreeMap::from([("team.example/owner".into(), "victim".into())]),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some("server_label_refused"));
    // Newly submitted bare dotted keys fail on the real registration endpoint.
    let error = attacker
        .register(&RegisterRequest {
            name: "attacker".into(),
            labels: BTreeMap::from([("infact.kind".into(), "x".into())]),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some("invalid_label"));
    drop(http);
}

#[tokio::test]
async fn stale_epoch_is_fenced_on_same_id_over_http() {
    let (server, _http, client) = setup().await;
    let first = claimed(&server, &client, "reuse").await;
    let old = client.with_lease_epoch(&first.id, first.epoch);
    server.expire_lease_reusing_id(&first.id);
    let second = client.claim(Some("reuse"), 0).await.unwrap().unwrap();
    assert_eq!(first.id, second.id);
    assert!(second.epoch > first.epoch);
    for error in [
        old.heartbeat(&first.id, &heartbeat()).await.unwrap_err(),
        old.events(&first.id, vec![json!({"attempt":1,"sequence":1})])
            .await
            .unwrap_err(),
        old.credentials(&first.id).await.unwrap_err(),
        old.result(&first.id, &result()).await.unwrap_err(),
        old.release(
            &first.id,
            &ReleaseRequest::<Value> {
                reason: ReleaseReason::Error,
                message: None,
                result: None,
            },
        )
        .await
        .unwrap_err(),
        old.open_terminal(&first.id).await.unwrap_err(),
    ] {
        assert!(error.is_lease_lost(), "{error:?}");
    }
    client.heartbeat(&second.id, &heartbeat()).await.unwrap();
    client.result(&second.id, &result()).await.unwrap();
}

#[tokio::test]
async fn mock_worker_release_requeues_and_vanish_expires_without_reporting() {
    use awman_wsp::mock_worker::{MockOutcome, MockWorker};
    let (server, _http, client) = setup().await;
    let mut worker = MockWorker::new(
        client.clone(),
        "scripted",
        WorkerLabels::new(BTreeMap::new(), BTreeMap::new()).unwrap(),
        vec![],
    );
    server
        .enqueue(work_item("release_vanish"), Utc::now())
        .unwrap();
    let released = worker
        .run_once(MockOutcome::Release(ReleaseReason::Shutdown))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        server.release_for(&released.lease.id).unwrap().reason,
        ReleaseReason::Shutdown
    );
    assert_eq!(client.queue(50, None).await.unwrap().items.len(), 1);
    let vanished = worker.run_once(MockOutcome::Vanish).await.unwrap().unwrap();
    assert!(server.heartbeat_for(&vanished.lease.id).is_none());
    assert!(server.result_for("release_vanish").is_none());
    assert!(server.release_for(&vanished.lease.id).is_none());
    assert!(client.queue(50, None).await.unwrap().items.is_empty());
    // Deterministic future clock exercises actual expiry, without a sleep.
    let registration = worker.registration.unwrap();
    let queue = server
        .queue_at(
            &registration.worker_id,
            vanished.lease.expires_at + Duration::milliseconds(1),
            50,
            None,
        )
        .unwrap();
    assert_eq!(queue.items[0].id, "release_vanish");
    assert!(client
        .heartbeat(&vanished.lease.id, &heartbeat())
        .await
        .unwrap_err()
        .is_lease_lost());
}
