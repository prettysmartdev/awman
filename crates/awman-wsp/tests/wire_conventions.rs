use awman_wsp::{
    labels::{validate_key, WorkerLabels},
    terminal::{LeaseTerminalFrame, TerminalFrame},
    *,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
fn value(v: impl Serialize) -> Value {
    serde_json::to_value(v).unwrap()
}
fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-09T12:00:00.123456Z")
        .unwrap()
        .with_timezone(&Utc)
}
fn messages() -> Vec<Value> {
    let item = conformance::work_item("fixture");
    let frame = |frame| {
        value(LeaseTerminalFrame {
            terminal_id: "term_1".into(),
            frame,
        })
    };
    vec![
        value(LinkStart {
            user_code: "ABCD".into(),
            verification_uri: "https://example.invalid".into(),
            link_id: "link_1".into(),
            interval_ms: 250,
        }),
        value(LinkToken {
            refresh_token: "secret".into(),
        }),
        value(TokenRequest {
            refresh_token: "secret".into(),
        }),
        value(AccessToken {
            access_token: "secret".into(),
            expires_at: Some(at()),
        }),
        value(RegisterRequest {
            name: "worker".into(),
            labels: BTreeMap::new(),
            capacity: 1,
            features: features::ALL.iter().map(|s| s.to_string()).collect(),
        }),
        value(RegisterResponse {
            worker_id: "worker_1".into(),
            server_labels: BTreeMap::new(),
            heartbeat_ms: 1000,
            poll_ms: 100,
        }),
        value(List {
            items: vec![QueueItem {
                body: None,
                source: None,
                id: "i".into(),
                title: "title".into(),
                summary: None,
                kind: WorkKind::Workflow,
                labels: BTreeMap::new(),
                priority: 1,
                enqueued_at: at(),
                assigned_to_me: true,
            }],
            next_cursor: None,
        }),
        value(item.clone()),
        value(item.repo),
        value(Credentials {
            kind: CredentialKind::Basic,
            token: Some("secret".into()),
            username: Some("user".into()),
            expires_at: Some(at()),
        }),
        value(ClaimRequest { item_id: None }),
        value(ClaimRequest {
            item_id: Some("i".into()),
        }),
        value(ClaimResponse {
            lease: Lease {
                id: "lease".into(),
                epoch: 1,
                expires_at: at(),
                item: conformance::work_item("i"),
                terminal: true,
                remote_input: false,
            },
        }),
        value(HeartbeatRequest {
            state: HeartbeatState::AwaitingInput,
            step: Some("step".into()),
            progress: Some(json!({"done":1})),
        }),
        value(HeartbeatResponse {
            expires_at: at(),
            cancel: true,
            cancel_reason: Some("requested".into()),
        }),
        value(EventBatch {
            events: vec![EventEnvelope {
                attempt: 1,
                sequence: 3,
                rest: json!({"type":"workflow_state","data":{}}),
            }],
        }),
        value(EventsResponse { accepted: 1 }),
        value(LeaseResult {
            result: json!({"outcome":"done","exit_code":0}),
            outputs: Some(BTreeMap::from([("result.txt".into(), "aGk=".into())])),
            outputs_mounted: true,
            duration_ms: 55,
        }),
        value(ReleaseRequest {
            reason: ReleaseReason::UserDeclined,
            message: Some("no".into()),
            result: Some(json!({"outcome":"paused"})),
        }),
        value(ApiError {
            error: "gone".into(),
            code: "lease_lost".into(),
        }),
        frame(TerminalFrame::Snapshot {
            has_output: false,
            cols: 80,
            rows: 24,
            screen: "aGk=".into(),
        }),
        frame(TerminalFrame::Output {
            bytes: "aGk=".into(),
        }),
        frame(TerminalFrame::Resize {
            cols: 120,
            rows: 40,
        }),
        frame(TerminalFrame::Exit { code: Some(0) }),
        frame(TerminalFrame::Lagged),
        frame(TerminalFrame::Error {
            code: "input_refused".into(),
            error: "refused".into(),
        }),
        frame(TerminalFrame::Input {
            bytes: "eA==".into(),
        }),
        frame(TerminalFrame::TakeControl),
        frame(TerminalFrame::Detached {
            reason: "user".into(),
        }),
        value(WorkKind::Prompt),
        value(Interaction::Headless),
        value(CredentialKind::None),
        value(ReleaseReason::Shutdown),
        value(ReleaseReason::Ineligible),
        value(ReleaseReason::Error),
    ]
}
use awman_wsp::conformance::walk;
#[test]
fn every_message_uses_snake_case_fields_and_enum_values() {
    for message in messages() {
        walk(&message, &mut |k, _| {
            assert!(
                k.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{k}"
            )
        });
    }
    for op in [
        MatchOp::In,
        MatchOp::NotIn,
        MatchOp::Exists,
        MatchOp::DoesNotExist,
    ] {
        // Affinity operators are the explicit Kubernetes-spelling exception in WSP.
        assert_eq!(value(op), format!("{op:?}"));
    }
    assert_eq!(value(HeartbeatState::AwaitingInput), "awaiting_input");
    assert_eq!(value(ReleaseReason::UserDeclined), "user_declined");
    assert_eq!(value(Interaction::Interactive), "interactive");
    assert_eq!(value(WorkKind::Workflow), "workflow");
}
#[test]
fn every_list_response_has_items_and_next_cursor() {
    assert_eq!(
        value(List::<QueueItem> {
            items: vec![],
            next_cursor: None
        }),
        json!({"items":[],"next_cursor":null})
    );
    let list = messages()
        .into_iter()
        .find(|v| v.get("items").is_some())
        .unwrap();
    assert!(list["items"].is_array());
    assert!(list.get("next_cursor").is_some());
}
#[test]
fn duration_fields_end_in_ms_and_are_integers() {
    let mut count = 0;
    for message in messages() {
        walk(&message, &mut |k, v| {
            if k.ends_with("_ms") {
                assert!(v.is_null() || v.is_u64(), "{k}: {v}");
                count += 1;
            }
        });
    }
    assert!(count >= 5);
    #[derive(Serialize, Deserialize)]
    struct DurationWire {
        #[serde(with = "wire::duration_ms")]
        duration_ms: std::time::Duration,
    }
    assert_eq!(
        value(DurationWire {
            duration_ms: std::time::Duration::from_millis(23)
        }),
        json!({"duration_ms":23})
    );
}
#[test]
fn timestamp_fields_end_in_at_and_roundtrip_rfc3339() {
    let mut count = 0;
    for message in messages() {
        walk(&message, &mut |k, v| {
            if k.ends_with("_at") && !v.is_null() {
                let text = v.as_str().unwrap();
                DateTime::parse_from_rfc3339(text).unwrap();
                assert_eq!(text, "2026-11-09T12:00:00.123Z");
                count += 1;
            }
        });
    }
    assert!(count >= 5);
    let token: AccessToken = serde_json::from_value(
        json!({"access_token":"x","expires_at":"2026-11-09T13:00:00+01:00"}),
    )
    .unwrap();
    assert_eq!(value(token)["expires_at"], "2026-11-09T12:00:00.000Z");
}
#[test]
fn every_error_response_has_error_and_snake_case_code() {
    for code in [
        codes::TAKEN,
        codes::LEASE_LOST,
        codes::INELIGIBLE,
        codes::NOT_FOUND,
        codes::UNAUTHORIZED,
        codes::INVALID_EVENT,
        codes::LINK_PENDING,
        codes::LINK_DENIED,
    ] {
        let error = ApiError {
            error: "error".into(),
            code: code.into(),
        };
        assert_eq!(value(&error), json!({"error":"error","code":code}));
        assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        assert_eq!(
            error.status(),
            match code {
                "taken" | "lease_lost" | "ineligible" => 409,
                "not_found" => 404,
                "unauthorized" => 401,
                _ => 400,
            }
        );
    }
}
#[test]
fn label_key_validation_and_provenance_enforce_reserved_prefix() {
    for key in [
        "pool",
        "owner",
        "awman/agent.codex",
        "team.example/owner",
        "a-b/c.d",
    ] {
        validate_key(key).unwrap();
    }
    for key in ["", "UPPER", "a/b/c", "a_b", "/name", "space key", "ns/"] {
        assert!(validate_key(key).is_err(), "{key}");
    }
    let reported = BTreeMap::from([("awman/os".into(), "linux".into())]);
    assert!(WorkerLabels::new(
        reported.clone(),
        BTreeMap::from([("awman/os".into(), "fake".into())])
    )
    .is_err());
    assert!(WorkerLabels::new(
        reported.clone(),
        BTreeMap::from([("owner".into(), "fake".into())])
    )
    .unwrap()
    .with_server(BTreeMap::from([("owner".into(), "alice".into())]))
    .is_err());
    let labels = WorkerLabels::new(reported, BTreeMap::new())
        .unwrap()
        .with_server(BTreeMap::from([("owner".into(), "alice".into())]))
        .unwrap();
    assert_eq!(labels.merged()["owner"], "alice");
    assert_eq!(labels.merged()["awman/os"], "linux");
}
#[test]
fn token_and_credential_debug_always_redacts_secrets() {
    let secret = "DO_NOT_PRINT_THIS";
    for debug in [
        format!(
            "{:?}",
            LinkToken {
                refresh_token: secret.into()
            }
        ),
        format!(
            "{:?}",
            TokenRequest {
                refresh_token: secret.into()
            }
        ),
        format!(
            "{:?}",
            AccessToken {
                access_token: secret.into(),
                expires_at: None
            }
        ),
        format!(
            "{:?}",
            Credentials {
                kind: CredentialKind::Basic,
                token: Some(secret.into()),
                username: Some(secret.into()),
                expires_at: None
            }
        ),
        format!(
            "{:?}",
            Client::new("https://example.invalid", secret.into()).unwrap()
        ),
        format!(
            "{:?}",
            Error::Server {
                status: reqwest::StatusCode::BAD_REQUEST,
                code: "bad".into(),
                message: secret.into()
            }
        ),
    ] {
        assert!(!debug.contains(secret));
        assert!(debug.contains("redacted"));
    }
}
#[test]
fn generic_result_preserves_every_command_result_field_and_extension() {
    let command = json!({"exit_code":1,"outcome":"error","failure":{"stage":"git_auth","code":"git_auth_failed","message":"denied"},"failed_steps":[{"step":"test","attempt":1}],"git":{"pushed":false},"pushed":{"sha":"abc"},"pull_request":{"url":"https://example.invalid/1"},"final_message":"failed","future_field":{"anything":true}});
    let raw = json!({"result":command,"outputs_mounted":false,"duration_ms":12});
    let decoded: LeaseResult<Value> = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(value(decoded), raw);
}

#[test]
fn worker_cannot_spoof_server_owned_labels_via_reported_facts() {
    let labels = WorkerLabels::new(
        BTreeMap::from([("owner".into(), "mallory".into())]),
        BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        labels
            .with_server(BTreeMap::from([("owner".into(), "alice".into())]))
            .unwrap_err(),
        awman_wsp::labels::LabelError::ServerOwned
    );
}
