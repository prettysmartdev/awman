use super::support::{prompt, AdmissionFixture};
use awman::command::commands::api_server::admission::{
    AdmissionError, IdempotencyGate, IdempotentResult,
};
use awman::data::fs::api_db::{IdempotencyBegin, SessionCommandAdmission};
use awman::data::session_setup_event::SessionSetupStatus;
use awman::data::session_status::SessionStatus;

#[tokio::test(flavor = "multi_thread")]
async fn validation_and_authorization_leave_no_durable_work() {
    let s = crate::support::sandbox(
        "command::admission::validation_and_authorization_leave_no_durable_work",
    );
    let f = AdmissionFixture::new(&s);
    let mut forbidden = prompt("ignored");
    forbidden.subcommand = "clean".into();
    forbidden.args.clear();
    assert!(matches!(
        f.admission
            .admit_session(f.local(&s), Some(forbidden), false, None)
            .await,
        Err(AdmissionError::Unavailable { .. })
    ));
    let mut invalid = f.local(&s);
    invalid.request.workdir = Some(s.home().to_string_lossy().into_owned());
    assert!(matches!(
        f.admission.admit_session(invalid, None, false, None).await,
        Err(AdmissionError::SessionCreation(_))
    ));
    assert!(f.store.list_sessions().unwrap().is_empty());
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_and_closed_sessions_refuse_command_admission() {
    let s = crate::support::sandbox(
        "command::admission::setup_and_closed_sessions_refuse_command_admission",
    );
    let f = AdmissionFixture::new(&s);
    assert!(matches!(
        f.admission
            .admit_command("missing", prompt("p"), None)
            .await,
        Err(AdmissionError::Session(SessionCommandAdmission::NotFound))
    ));
    let session = f
        .admission
        .admit_session(f.local(&s), None, false, None)
        .await
        .unwrap();
    assert!(matches!(
        f.admission
            .admit_command(&session.session_id, prompt("p"), None)
            .await,
        Err(AdmissionError::Setup(_))
    ));
    f.store
        .update_setup_status(&session.session_id, SessionSetupStatus::Ready)
        .unwrap();
    let command = f
        .admission
        .admit_command(&session.session_id, prompt("literal $(codex)"), None)
        .await
        .unwrap();
    let row = f.store.get_command(&command.command_id).unwrap().unwrap();
    assert!(row
        .decode_args()
        .unwrap()
        .contains(&"literal $(codex)".to_string()));
    assert_eq!(command.flags_applied["headless"], true);
    f.store
        .update_session_status(&session.session_id, SessionStatus::Closed)
        .unwrap();
    assert!(matches!(
        f.admission
            .admit_command(&session.session_id, prompt("p"), None)
            .await,
        Err(AdmissionError::Session(SessionCommandAdmission::Closed))
    ));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn accepted_session_replays_after_dropped_response_without_duplication() {
    let s = crate::support::sandbox(
        "command::admission::accepted_session_replays_after_dropped_response_without_duplication",
    );
    let f = AdmissionFixture::new(&s);
    let journal = f.journal("retry");
    assert_eq!(journal.begin("body").unwrap(), IdempotencyBegin::Fresh);
    let session = f
        .admission
        .admit_session(f.local(&s), Some(prompt("p")), true, Some(&journal))
        .await
        .unwrap();
    drop(journal);
    let retry = f.journal("retry");
    assert_eq!(
        retry.begin("body").unwrap(),
        IdempotencyBegin::Replay {
            status: 202,
            body: session.response_body
        }
    );
    assert_eq!(f.store.list_sessions().unwrap().len(), 1);
    assert_eq!(
        f.store
            .list_commands_for_session(&session.session_id, 100)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        retry.begin("different").unwrap(),
        IdempotencyBegin::Mismatch
    );
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn write_failure_compensates_before_a_key_can_be_retried() {
    let s = crate::support::sandbox(
        "command::admission::write_failure_compensates_before_a_key_can_be_retried",
    );
    let f = AdmissionFixture::new(&s);
    f.sql("CREATE TRIGGER fail_labels BEFORE INSERT ON labels BEGIN SELECT RAISE(ABORT, 'injected labels failure'); END;");
    let journal = f.journal("failure");
    assert_eq!(journal.begin("body").unwrap(), IdempotencyBegin::Fresh);
    assert!(f
        .admission
        .admit_session(f.local(&s), Some(prompt("p")), true, Some(&journal))
        .await
        .is_err());
    assert!(f
        .store
        .list_sessions()
        .unwrap()
        .iter()
        .all(|r| r.status == SessionStatus::Closed));
    drop(journal);
    f.sql("DROP TRIGGER fail_labels;");
    let retry = f.journal("failure");
    assert_eq!(retry.begin("body").unwrap(), IdempotencyBegin::Fresh);
    let accepted = f
        .admission
        .admit_session(f.local(&s), None, false, Some(&retry))
        .await
        .unwrap();
    assert_eq!(
        f.store
            .get_session(&accepted.session_id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Active
    );
    s.finish();
}

#[test]
fn reservations_are_scoped_and_only_the_owner_can_release_them() {
    let s = crate::support::sandbox(
        "command::admission::reservations_are_scoped_and_only_the_owner_can_release_them",
    );
    let f = AdmissionFixture::new(&s);
    let first = f.journal("shared");
    assert_eq!(first.begin("body").unwrap(), IdempotencyBegin::Fresh);
    let other = f.journal("shared");
    assert_eq!(other.begin("body").unwrap(), IdempotencyBegin::InFlight);
    drop(other);
    assert_eq!(
        f.journal("shared").begin("body").unwrap(),
        IdempotencyBegin::InFlight
    );
    let principal = awman::command::commands::api_server::admission::IdempotentAdmission::new(
        f.store.clone(),
        "sessions",
        "another",
        "shared",
    );
    assert_eq!(principal.begin("body").unwrap(), IdempotencyBegin::Fresh);
    assert!(first.complete(400, "rejected").unwrap());
    first.release().unwrap();
    assert_eq!(
        f.journal("shared").begin("body").unwrap(),
        IdempotencyBegin::Replay {
            status: 400,
            body: "rejected".into()
        }
    );
    s.finish();
}

#[test]
fn idempotency_key_boundaries_and_server_failure_release_are_explicit() {
    let s = crate::support::sandbox(
        "command::admission::idempotency_key_boundaries_and_server_failure_release_are_explicit",
    );
    let f = AdmissionFixture::new(&s);
    for key in ["", "space key", "\n", "é"] {
        assert!(IdempotencyGate::open(&f.store, "route", "p", Some(key), "body").is_err());
    }
    let gate = IdempotencyGate::open(&f.store, "route", "p", Some("visible!"), "body").unwrap();
    let IdempotencyGate::Fresh(run) = gate else {
        panic!("expected reservation")
    };
    run.finish(IdempotentResult::ServerError);
    assert!(matches!(
        IdempotencyGate::open(&f.store, "route", "p", Some("visible!"), "body").unwrap(),
        IdempotencyGate::Fresh(_)
    ));
    s.finish();
}
