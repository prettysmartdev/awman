//! `EngineInstanceHost` unit tests (WI 0142 Part A §2–§4, §8): the lifecycle
//! state machine end to end (quit, primary loss with grace, secondary loss,
//! close last tab), admission, first-answer-wins dialogs across three
//! frontends, presenter routing and fallback, namespace ownership, and the
//! `Hello.env` secret never reaching logs, frames or the store.
//!
//! Instances are hermetic: the test opener builds `Engines::for_tests` and a
//! session on a temp directory, and opens the first tab in its fatal-start
//! form so no startup command (and no runtime) runs. Commands that raise
//! dialogs are hand-built channel sets bound to a tab.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::command::dispatch::{Engines, LaunchDisplay};
use crate::command::instance::feeds::{ContainerSlotIo, TabFeeds};
use crate::command::instance::frontend::PlainPresentation;
use crate::command::instance::SubmittedCommand;
use crate::command::integrations::{HostRequirement, Integration, PresenterHandle};
use crate::data::annotations::AnnotationFilter;
use crate::data::config::effective::EffectiveConfig;
use crate::data::dialog::DialogRequest;
use crate::data::frontend_protocol::{
    DetachReasonWire, FrontendRole, HelloEnv, PRIMARY_RECONNECT_GRACE,
};
use crate::data::session::{Session, StaticGitRootResolver};
use crate::data::wire::ListQuery;

const SECRET: &str = "ghp_s3cret-value-never-logged";

struct TestOpener {
    root: PathBuf,
}

impl InstanceOpener for TestOpener {
    fn open(&self, request: OpenInstanceRequest) -> Result<OpenedInstance, CommandError> {
        let cwd = request.env.cwd().to_path_buf();
        let session = Session::open(
            cwd.clone(),
            &StaticGitRootResolver::new(cwd),
            SessionOpenOptions {
                env: Some(request.env.clone()),
                ..Default::default()
            },
        )?;
        let env = request.env.clone();
        let instance = EngineInstance::new_hosted(
            InstanceParts {
                catalogue: request.catalogue,
                engines: Engines::for_tests(&self.root),
                session_manager: Arc::new(SessionManager::in_memory(request.env.clone())),
                runtime: tokio::runtime::Handle::current(),
                env: Arc::new(move || env.clone()),
                presentation: Arc::new(PlainPresentation),
            },
            HostedOptions {
                id: request.id,
                kind: FrontendKind::ENGINE_LOCAL,
                hub: Some(request.hub),
                executor: request.executor,
            },
        );
        Ok(OpenedInstance {
            instance,
            initial: InitialTabSpec::Session {
                session,
                // Opens the tab without a startup command.
                fatal_startup_error: true,
                pty_size: (request.term_size.cols, request.term_size.rows),
            },
            notices: Vec::new(),
        })
    }
}

/// A presenter for the `rex` namespace that records what it sees.
#[derive(Default)]
struct RecordingPresenter {
    panics: bool,
    handle: Mutex<Option<PresenterHandle>>,
    events: Mutex<Vec<PresenterEvent>>,
    requests: Mutex<Vec<UiRequest>>,
}

impl HostPresenter for RecordingPresenter {
    fn name(&self) -> &'static str {
        "rex"
    }
    fn bind(&self, handle: PresenterHandle) {
        *self.handle.lock().unwrap() = Some(handle);
    }
    fn on_ui_request(
        &self,
        _instance: &InstanceId,
        _frontend: &FrontendId,
        req: &UiRequest,
    ) -> UiOutcome {
        if self.panics {
            panic!("presenter bug");
        }
        self.requests.lock().unwrap().push(req.clone());
        UiOutcome::claimed("rex")
    }
    fn on_instance_event(&self, _instance: &InstanceId, ev: &PresenterEvent) {
        self.events.lock().unwrap().push(ev.clone());
    }
}

struct RexIntegration(Arc<RecordingPresenter>);

impl Integration for RexIntegration {
    fn name(&self) -> &'static str {
        "rex"
    }
    fn detect(&self, _env: &EnvSnapshot, _config: &EffectiveConfig) -> HostRequirement {
        HostRequirement::None
    }
    fn presenter(&self) -> Option<Arc<dyn HostPresenter>> {
        Some(self.0.clone())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    host: Arc<EngineInstanceHost>,
}

fn fixture_with(registry: Arc<IntegrationRegistry>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("repo")).unwrap();
    let host = EngineInstanceHost::new(HostParts {
        hub: Arc::new(TerminalHub::new(Default::default())),
        catalogue: CommandCatalogue::get(),
        registry,
        opener: Arc::new(TestOpener { root: root.clone() }),
        executor: None,
        build: "test-build".into(),
    });
    Fixture {
        _dir: dir,
        root,
        host,
    }
}

fn fixture() -> Fixture {
    fixture_with(IntegrationRegistry::compiled_in())
}

impl Fixture {
    fn hello(&self, role: FrontendRole) -> Hello {
        let home = self.root.join("home");
        Hello {
            protocol: FRONTEND_SUBPROTOCOL.into(),
            build: "test-build".into(),
            kind: FrontendKindWire::Tui,
            role,
            annotations: Annotations::new(),
            cwd: self.root.join("repo"),
            argv: vec!["awman".into()],
            env: HelloEnv::from_pairs([
                ("HOME", home.display().to_string()),
                ("AWMAN_HOME", home.join(".awman").display().to_string()),
                ("PATH", "/usr/bin:/bin".to_string()),
                ("GITHUB_TOKEN", SECRET.to_string()),
            ]),
            term_size: TermSize {
                cols: 100,
                rows: 30,
            },
            stdin_is_tty: true,
            stdout_is_tty: true,
            view: None,
            resume: None,
        }
    }

    async fn attach(
        &self,
        id: &InstanceId,
        hello: Hello,
    ) -> Result<(FrontendSession, tokio::sync::mpsc::Receiver<EngineFrame>), AttachRefusal> {
        let (tx, rx) = tokio::sync::mpsc::channel(4096);
        let session = self.host.attach_frontend(id.clone(), hello, tx).await?;
        Ok((session, rx))
    }

    /// A started instance with its primary.
    async fn started(
        &self,
    ) -> (
        InstanceId,
        FrontendSession,
        tokio::sync::mpsc::Receiver<EngineFrame>,
    ) {
        let id = self.host.create_instance(Annotations::new()).unwrap();
        let (primary, rx) = self
            .attach(&id, self.hello(FrontendRole::Primary))
            .await
            .unwrap();
        (id, primary, rx)
    }

    fn first_tab(&self, id: &InstanceId) -> TabId {
        self.host.tabs(id).unwrap()[0].tab_id.clone()
    }
}

fn drain(rx: &mut tokio::sync::mpsc::Receiver<EngineFrame>) -> Vec<EngineFrame> {
    let mut frames = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        frames.push(frame);
    }
    frames
}

fn detaches(frames: &[EngineFrame]) -> Vec<DetachReasonWire> {
    frames
        .iter()
        .filter_map(|f| match f {
            EngineFrame::Detach(reason) => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

/// A hand-built command: the command side of the dialog channels.
struct FakeCommand {
    requests: std::sync::mpsc::Sender<DialogRequest>,
    responses: std::sync::mpsc::Receiver<DialogResponse>,
    _result:
        std::sync::mpsc::Sender<Result<crate::command::dispatch::CommandOutcome, CommandError>>,
    _stdout: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

fn bind_fake_command(fx: &Fixture, id: &InstanceId, tab: &TabId) -> FakeCommand {
    let (req_tx, req_rx) = std::sync::mpsc::channel();
    let (resp_tx, resp_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, _resize_rx) = tokio::sync::mpsc::unbounded_channel();
    fx.host.bind_test_command(
        id,
        tab,
        SubmittedCommand {
            display: LaunchDisplay {
                agent_display_name: "claude".into(),
                unattended: false,
            },
            sandboxed: false,
            feeds: TabFeeds {
                command_result_rx: result_rx,
                dialog_request_rx: req_rx,
                dialog_response_tx: resp_tx,
            },
            slot_io: ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        },
    );
    FakeCommand {
        requests: req_tx,
        responses: resp_rx,
        _result: result_tx,
        _stdout: stdout_tx,
    }
}

fn dialog_ids(frames: &[EngineFrame]) -> Vec<DialogId> {
    frames
        .iter()
        .filter_map(|f| match f {
            EngineFrame::DialogRequest { dialog_id, .. } => Some(dialog_id.clone()),
            _ => None,
        })
        .collect()
}

fn closed_by(frames: &[EngineFrame]) -> Vec<(DialogId, AnsweredBy)> {
    frames
        .iter()
        .filter_map(|f| match f {
            EngineFrame::DialogClosed {
                dialog_id,
                answered_by,
            } => Some((dialog_id.clone(), answered_by.clone())),
            _ => None,
        })
        .collect()
}

// ─── Lifecycle ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_primary_gets_welcome_then_a_snapshot_with_its_first_tab() {
    let fx = fixture();
    let (id, primary, mut rx) = fx.started().await;
    let frames = drain(&mut rx);
    match &frames[0] {
        EngineFrame::Welcome {
            frontend_id,
            instance_id,
            role,
            ..
        } => {
            assert_eq!(frontend_id, &primary.frontend_id);
            assert_eq!(instance_id, &id);
            assert_eq!(*role, FrontendRole::Primary);
        }
        other => panic!("first frame must be Welcome, got {other:?}"),
    }
    match &frames[1] {
        EngineFrame::Snapshot(snapshot) => {
            assert_eq!(snapshot.tabs.len(), 1);
            assert_eq!(snapshot.primary_frontend_kind, Some(FrontendKindWire::Tui));
        }
        other => panic!("second frame must be the snapshot, got {other:?}"),
    }
    let instance = fx.host.find(&id).unwrap().state().instance.clone().unwrap();
    assert_eq!(instance.kind(), FrontendKind::ENGINE_LOCAL);
}

#[tokio::test]
async fn quit_runs_the_exit_policy_and_detaches_every_frontend_with_quit() {
    let fx = fixture();
    let (id, primary, mut prx) = fx.started().await;
    let (_secondary, mut srx) = fx
        .attach(&id, fx.hello(FrontendRole::Secondary))
        .await
        .unwrap();
    drain(&mut prx);
    drain(&mut srx);
    primary.handle(1, FrontendFrame::Quit).await.unwrap();
    assert_eq!(detaches(&drain(&mut prx)), vec![DetachReasonWire::Quit]);
    assert_eq!(detaches(&drain(&mut srx)), vec![DetachReasonWire::Quit]);
    assert!(fx.host.get(&id).is_none());
    assert_eq!(fx.host.instance_count(), 0);
    assert!(EngineHost::activity(fx.host.as_ref()).is_idle());
}

#[tokio::test]
async fn primary_loss_quits_after_the_grace_and_a_reconnect_inside_it_keeps_the_instance() {
    let fx = fixture();
    let (id, primary, _prx) = fx.started().await;
    let (_secondary, mut srx) = fx
        .attach(&id, fx.hello(FrontendRole::Secondary))
        .await
        .unwrap();
    let primary_id = primary.frontend_id.clone();

    // A transient socket error: back within the grace under the same id.
    primary.disconnected();
    let activity = EngineHost::activity(fx.host.as_ref());
    assert_eq!(activity.reconnecting_frontends, 1);
    assert!(!activity.is_idle());
    let mut back = fx.hello(FrontendRole::Primary);
    back.resume = Some(primary_id.clone());
    let (primary, mut prx) = fx.attach(&id, back).await.unwrap();
    assert_eq!(primary.frontend_id, primary_id);
    assert!(matches!(
        drain(&mut prx).first(),
        Some(EngineFrame::Welcome { .. })
    ));
    fx.host
        .pump_at(Instant::now() + PRIMARY_RECONNECT_GRACE * 3);
    assert!(
        fx.host.get(&id).is_some(),
        "a resumed primary keeps the instance"
    );

    // Killed for good: today's killed-TUI policy after the grace, no prompt.
    let lost_at = Instant::now();
    primary.disconnected();
    fx.host.pump_at(lost_at + Duration::from_millis(1500));
    assert!(fx.host.get(&id).is_some(), "still inside the grace");
    drain(&mut srx);
    fx.host
        .pump_at(lost_at + PRIMARY_RECONNECT_GRACE + Duration::from_millis(10));
    assert!(fx.host.get(&id).is_none());
    assert_eq!(
        detaches(&drain(&mut srx)),
        vec![DetachReasonWire::InstanceEnded]
    );
}

#[tokio::test]
async fn secondary_loss_changes_nothing_but_the_frontend_list() {
    let fx = fixture();
    let (id, _primary, mut prx) = fx.started().await;
    let (secondary, _srx) = fx
        .attach(&id, fx.hello(FrontendRole::Secondary))
        .await
        .unwrap();
    let secondary_id = secondary.frontend_id.clone();
    drain(&mut prx);
    drop(secondary);
    let frames = drain(&mut prx);
    assert!(frames.iter().any(|f| matches!(
        f,
        EngineFrame::Delta(InstanceDelta::FrontendDetached { frontend_id, .. }) if frontend_id == &secondary_id
    )));
    assert!(detaches(&frames).is_empty());
    fx.host.pump_at(Instant::now() + Duration::from_secs(60));
    let detail = fx.host.get(&id).expect("instance survives a secondary");
    assert_eq!(detail.frontends.len(), 1);
}

#[tokio::test]
async fn closing_the_last_tab_quits_the_instance() {
    let fx = fixture();
    let (id, primary, mut prx) = fx.started().await;
    let first = fx.first_tab(&id);
    let second = fx
        .host
        .open_tab(
            &id,
            Some(&primary.frontend_id),
            fx.root.join("repo"),
            Default::default(),
        )
        .unwrap();
    assert_eq!(fx.host.tabs(&id).unwrap().len(), 2);
    drain(&mut prx);
    primary
        .handle(
            1,
            FrontendFrame::CloseTab {
                tab_id: first.clone(),
            },
        )
        .await
        .unwrap();
    let frames = drain(&mut prx);
    assert!(frames.iter().any(|f| matches!(
        f,
        EngineFrame::Delta(InstanceDelta::TabClosed { tab_id }) if tab_id == &first
    )));
    assert!(detaches(&frames).is_empty());
    primary
        .handle(2, FrontendFrame::CloseTab { tab_id: second })
        .await
        .unwrap();
    assert_eq!(detaches(&drain(&mut prx)), vec![DetachReasonWire::Quit]);
    assert!(fx.host.get(&id).is_none());
}

#[tokio::test]
async fn delete_quits_like_a_frontend_quit() {
    let fx = fixture();
    let (id, _primary, mut prx) = fx.started().await;
    drain(&mut prx);
    let plan = fx.host.delete(&id).unwrap();
    assert!(plan.cancel_running);
    assert_eq!(detaches(&drain(&mut prx)), vec![DetachReasonWire::Quit]);
    assert!(matches!(fx.host.delete(&id), Err(HostError::NotFound)));
}

#[tokio::test]
async fn a_reserved_instance_without_its_primary_is_discarded() {
    let fx = fixture();
    let id = fx.host.create_instance(Annotations::new()).unwrap();
    assert_eq!(fx.host.instance_count(), 1);
    fx.host.pump_at(
        Instant::now() + super::super::lifecycle::PRIMARY_HELLO_TIMEOUT + Duration::from_secs(1),
    );
    assert_eq!(fx.host.instance_count(), 0);
    assert!(matches!(
        fx.attach(&id, fx.hello(FrontendRole::Primary)).await,
        Err(AttachRefusal::NotFound)
    ));
}

#[tokio::test]
async fn draining_detaches_every_frontend_with_engine_disabled() {
    let fx = fixture();
    let (id, _primary, mut prx) = fx.started().await;
    drain(&mut prx);
    EngineHost::drain(fx.host.as_ref(), EngineStopReason::Disabled).await;
    assert_eq!(
        detaches(&drain(&mut prx)),
        vec![DetachReasonWire::EngineDisabled]
    );
    assert!(fx.host.get(&id).is_none());
    assert!(matches!(
        fx.host.create_instance(Annotations::new()),
        Err(HostError::Draining)
    ));
    EngineHost::resume(fx.host.as_ref());
    assert!(fx.host.create_instance(Annotations::new()).is_ok());
}

// ─── Admission ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_second_primary_is_refused_with_primary_exists() {
    let fx = fixture();
    let (id, _primary, _rx) = fx.started().await;
    let refusal = fx
        .attach(&id, fx.hello(FrontendRole::Primary))
        .await
        .unwrap_err();
    assert_eq!(refusal, AttachRefusal::PrimaryExists);
    assert_eq!(refusal.code(), "primary_exists");
    // Any number of secondaries, for one tab or all.
    let mut view = fx.hello(FrontendRole::Secondary);
    view.view = Some(crate::data::frontend_protocol::ViewScope::Tab(
        fx.first_tab(&id),
    ));
    let _one_tab = fx.attach(&id, view).await.unwrap();
    let _all_tabs = fx
        .attach(&id, fx.hello(FrontendRole::Secondary))
        .await
        .unwrap();
    assert_eq!(fx.host.get(&id).unwrap().frontends.len(), 3);
    let mut unknown = fx.hello(FrontendRole::Secondary);
    unknown.view = Some(crate::data::frontend_protocol::ViewScope::Tab(TabId::new()));
    assert_eq!(
        fx.attach(&id, unknown).await.unwrap_err(),
        AttachRefusal::UnknownTab
    );
}

// ─── Dialogs ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn three_frontends_get_every_dialog_and_the_first_answer_wins() {
    let fx = fixture();
    let (id, a, mut arx) = fx.started().await;
    let (b, mut brx) = fx
        .attach(&id, fx.hello(FrontendRole::Secondary))
        .await
        .unwrap();
    let (c, mut crx) = fx
        .attach(&id, fx.hello(FrontendRole::Secondary))
        .await
        .unwrap();
    let tab = fx.first_tab(&id);
    let command = bind_fake_command(&fx, &id, &tab);
    for rx in [&mut arx, &mut brx, &mut crx] {
        drain(rx);
    }

    command
        .requests
        .send(DialogRequest::YesNo {
            title: "Proceed?".into(),
            body: "body".into(),
        })
        .unwrap();
    fx.host.pump_at(Instant::now());
    let (fa, fb, fc) = (drain(&mut arx), drain(&mut brx), drain(&mut crx));
    let dialog = dialog_ids(&fa)[0].clone();
    assert_eq!(dialog_ids(&fb), vec![dialog.clone()]);
    assert_eq!(dialog_ids(&fc), vec![dialog.clone()]);
    assert_eq!(fx.host.dialogs(&id).unwrap().len(), 1);

    // B answers first: the command gets B's answer, A and C are told B won.
    b.handle(
        1,
        FrontendFrame::DialogResponse {
            dialog_id: dialog.clone(),
            response: DialogResponse::Yes,
        },
    )
    .await
    .unwrap();
    assert_eq!(command.responses.try_recv().unwrap(), DialogResponse::Yes);
    let by_b = AnsweredBy::Frontend {
        frontend_id: b.frontend_id.clone(),
    };
    assert_eq!(
        closed_by(&drain(&mut arx)),
        vec![(dialog.clone(), by_b.clone())]
    );
    assert_eq!(
        closed_by(&drain(&mut crx)),
        vec![(dialog.clone(), by_b.clone())]
    );
    assert!(closed_by(&drain(&mut brx)).is_empty());

    // C's late answer loses and never reaches the command.
    c.handle(
        1,
        FrontendFrame::DialogResponse {
            dialog_id: dialog.clone(),
            response: DialogResponse::No,
        },
    )
    .await
    .unwrap();
    assert!(command.responses.try_recv().is_err());
    assert_eq!(
        fx.host
            .answer(&id, &dialog, DialogResponse::No, AnsweredBy::Api)
            .unwrap(),
        DialogAnswerOutcome::AlreadyAnswered { by: by_b }
    );
    assert!(fx.host.dialogs(&id).unwrap().is_empty());

    // A REST answer closes the dialog on every frontend.
    command
        .requests
        .send(DialogRequest::YesNo {
            title: "Again?".into(),
            body: String::new(),
        })
        .unwrap();
    fx.host.pump_at(Instant::now());
    let second = dialog_ids(&drain(&mut arx))[0].clone();
    drain(&mut brx);
    drain(&mut crx);
    assert_eq!(
        fx.host
            .answer(
                &id,
                &second,
                DialogResponse::Text("nope".into()),
                AnsweredBy::Api
            )
            .unwrap(),
        DialogAnswerOutcome::Rejected,
        "an answer that does not fit the request is refused"
    );
    assert_eq!(
        fx.host
            .answer(&id, &second, DialogResponse::No, AnsweredBy::Api)
            .unwrap(),
        DialogAnswerOutcome::Answered
    );
    assert_eq!(command.responses.try_recv().unwrap(), DialogResponse::No);
    for rx in [&mut arx, &mut brx, &mut crx] {
        assert_eq!(
            closed_by(&drain(rx)),
            vec![(second.clone(), AnsweredBy::Api)]
        );
    }
    drop(a);
}

#[tokio::test]
async fn a_bound_command_slot_is_a_hub_terminal_owned_by_its_tab() {
    let fx = fixture();
    let (id, _primary, _rx) = fx.started().await;
    let tab = fx.first_tab(&id);
    let _command = bind_fake_command(&fx, &id, &tab);
    let snapshot = fx.host.tab(&id, &tab).unwrap();
    let terminal = snapshot.slots[0]
        .terminal_id
        .clone()
        .expect("slot terminal");
    let record = fx.host.hub.get(&terminal).unwrap();
    assert_eq!(
        record.owner_id,
        crate::data::terminal::TerminalOwner::Tab(tab.clone())
    );
    assert_eq!((record.cols, record.rows), (100, 30), "the frontend's size");
    assert_eq!(fx.host.instance_for_tab(&tab), Some(id.clone()));
    assert_eq!(fx.host.instance_tab_ids(&id), Some(vec![tab]));
}

// ─── Presenters ─────────────────────────────────────────────────────────────

fn rex_fixture(panics: bool) -> (Fixture, Arc<RecordingPresenter>) {
    let presenter = Arc::new(RecordingPresenter {
        panics,
        ..Default::default()
    });
    let registry = IntegrationRegistry::with(vec![Arc::new(RexIntegration(presenter.clone()))]);
    (fixture_with(registry), presenter)
}

fn ui_acks(frames: &[EngineFrame]) -> Vec<(Seq, UiOutcome)> {
    frames
        .iter()
        .filter_map(|f| match f {
            EngineFrame::UiAck {
                request_seq,
                outcome,
            } => Some((*request_seq, outcome.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn ui_requests_go_to_the_presenter_claiming_the_frontends_namespace() {
    let (fx, presenter) = rex_fixture(false);
    let id = fx.host.create_instance(Annotations::new()).unwrap();
    let mut claimed = fx.hello(FrontendRole::Primary);
    claimed.annotations = Annotations::try_from_pairs([("rex/window", "w1")]).unwrap();
    let (rex_frontend, mut rx) = fx.attach(&id, claimed).await.unwrap();
    let (plain, mut plain_rx) = fx
        .attach(&id, fx.hello(FrontendRole::Secondary))
        .await
        .unwrap();
    drain(&mut rx);
    drain(&mut plain_rx);

    rex_frontend
        .handle(7, FrontendFrame::UiRequest(UiRequest::NextTab))
        .await
        .unwrap();
    let acks = ui_acks(&drain(&mut rx));
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].0, 7, "the ack names the request's seq");
    assert!(acks[0].1.claimed_by_host);
    assert_eq!(acks[0].1.claimed_by.as_deref(), Some("rex"));
    assert_eq!(
        *presenter.requests.lock().unwrap(),
        vec![UiRequest::NextTab]
    );

    // A frontend with no key in any namespace handles it natively.
    plain
        .handle(3, FrontendFrame::UiRequest(UiRequest::NextTab))
        .await
        .unwrap();
    let acks = ui_acks(&drain(&mut plain_rx));
    assert_eq!(acks, vec![(3, UiOutcome::unclaimed())]);
    // An API request goes to the presenter claiming the primary.
    let outcome = fx
        .host
        .ui_request(
            &id,
            None,
            UiRequest::OpenOverview {
                tab_id: fx.first_tab(&id),
            },
        )
        .await
        .unwrap();
    assert!(outcome.claimed_by_host);
    assert!(presenter
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, PresenterEvent::TabOpened { .. })));
}

#[tokio::test]
async fn a_panicking_presenter_falls_back_to_unclaimed_with_one_notice() {
    let (fx, _presenter) = rex_fixture(true);
    let id = fx.host.create_instance(Annotations::new()).unwrap();
    let mut claimed = fx.hello(FrontendRole::Primary);
    claimed.annotations = Annotations::try_from_pairs([("rex/window", "w1")]).unwrap();
    let (frontend, mut rx) = fx.attach(&id, claimed).await.unwrap();
    drain(&mut rx);
    frontend
        .handle(1, FrontendFrame::UiRequest(UiRequest::PrevTab))
        .await
        .unwrap();
    let frames = drain(&mut rx);
    assert_eq!(ui_acks(&frames), vec![(1, UiOutcome::unclaimed())]);
    let notices = frames
        .iter()
        .filter(|f| matches!(f, EngineFrame::Notice { .. }))
        .count();
    assert_eq!(notices, 1);
    assert!(fx.host.get(&id).is_some(), "the instance is unaffected");
}

#[tokio::test]
async fn presenters_write_only_their_own_namespace() {
    let (fx, presenter) = rex_fixture(false);
    let (id, _primary, mut rx) = fx.started().await;
    let tab = fx.first_tab(&id);
    drain(&mut rx);
    let handle = presenter.handle.lock().unwrap().clone().expect("bound");
    let target = AnnotationTarget::Tab {
        tab_id: tab.clone(),
    };
    handle.set(&target, "rex/window", "w9").unwrap();
    assert_eq!(
        fx.host
            .tab(&id, &tab)
            .unwrap()
            .annotations
            .get("rex/window"),
        Some("w9")
    );
    assert!(drain(&mut rx).iter().any(|f| matches!(
        f,
        EngineFrame::Delta(InstanceDelta::Annotations {
            target: AnnotationTarget::Tab { .. },
            ..
        })
    )));
    assert!(matches!(
        handle.set(&target, "other/window", "x"),
        Err(AnnotationError::ForeignNamespace { .. })
    ));
    assert!(matches!(
        handle.set(
            &AnnotationTarget::Tab {
                tab_id: TabId::new()
            },
            "rex/window",
            "x"
        ),
        Err(AnnotationError::NoTarget(_))
    ));
    // Outside a presenter callback no instance is in scope.
    assert!(matches!(
        handle.set(&AnnotationTarget::Instance, "rex/session", "s"),
        Err(AnnotationError::NoTarget(_))
    ));
    handle.clear(&target, "rex/window").unwrap();
    assert!(fx.host.tab(&id, &tab).unwrap().annotations.is_empty());
}

// ─── Annotations ────────────────────────────────────────────────────────────

#[tokio::test]
async fn annotations_filter_listing_and_end_with_the_instance() {
    let fx = fixture();
    let tagged = fx
        .host
        .create_instance(Annotations::try_from_pairs([("rex/window", "w1")]).unwrap())
        .unwrap();
    let _other = fx.host.create_instance(Annotations::new()).unwrap();
    let page = ListQuery::default().page().unwrap();
    let filter = AnnotationFilter::parse(&["rex/window=w1"]).unwrap();
    let listed = fx.host.list(&filter, &page);
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].id, tagged);
    assert_eq!(
        fx.host
            .list(&AnnotationFilter::default(), &page)
            .items
            .len(),
        2
    );

    let patched = fx
        .host
        .patch_annotations(
            &tagged,
            &Annotations::try_from_pairs([("rex/window", ""), ("rex/block", "b2")]).unwrap(),
        )
        .unwrap();
    assert_eq!(
        patched.get("rex/window"),
        None,
        "an empty value removes the key"
    );
    assert_eq!(patched.get("rex/block"), Some("b2"));
    assert!(fx.host.list(&filter, &page).items.is_empty());

    fx.host.delete(&tagged).ok();
    fx.host.quit_instance(&tagged, QuitReason::Api).ok();
    let filter = AnnotationFilter::parse(&["rex/block=b2"]).unwrap();
    assert!(fx.host.list(&filter, &page).items.is_empty());
}

// ─── Security: Hello.env ────────────────────────────────────────────────────

/// Callsites registered while no subscriber is set are cached as
/// uninteresting; a global subscriber answering `sometimes` keeps them
/// re-evaluated per dispatcher so the thread-local capture sees them.
fn keep_callsites_dynamic() {
    use tracing::span::{Attributes, Id, Record};
    use tracing::subscriber::Interest;
    use tracing::{Event, Metadata};
    struct Discard;
    impl tracing::Subscriber for Discard {
        fn register_callsite(&self, _: &Metadata<'_>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            false
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing::subscriber::set_global_default(Discard);
    });
}

/// Collect everything written to `tracing` on this thread while `body` runs.
fn capture_tracing<T>(body: impl FnOnce() -> T) -> (T, String) {
    keep_callsites_dynamic();
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let result = tracing::subscriber::with_default(subscriber, body);
    let bytes = sink.0.lock().unwrap().clone();
    (result, String::from_utf8_lossy(&bytes).into_owned())
}

fn files_containing(root: &std::path::Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path)
                .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
                .unwrap_or(false)
            {
                hits.push(path);
            }
        }
    }
    hits
}

#[test]
fn a_github_token_in_hello_env_never_reaches_logs_frames_events_or_the_store() {
    let fx = fixture();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let hello = fx.hello(FrontendRole::Primary);
    assert!(!format!("{hello:?}").contains(SECRET));
    assert!(format!("{hello:?}").contains("redacted"));

    let ((frames, json), logs) = capture_tracing(|| {
        runtime.block_on(async {
            let id = fx.host.create_instance(Annotations::new()).unwrap();
            let (primary, mut rx) = fx.attach(&id, hello.clone()).await.unwrap();
            let tab = fx.first_tab(&id);
            let command = bind_fake_command(&fx, &id, &tab);
            command
                .requests
                .send(DialogRequest::YesNo {
                    title: "t".into(),
                    body: "b".into(),
                })
                .unwrap();
            fx.host.pump_at(Instant::now());
            let env_debug = {
                let hosted = fx.host.find(&id).unwrap();
                let state = hosted.state();
                assert!(state.env.as_ref().unwrap().get("GITHUB_TOKEN").is_some());
                format!("{:?}", state.env)
            };
            let mut json = vec![
                env_debug,
                format!("{:?}", fx.host),
                format!("{primary:?}"),
                serde_json::to_string(&fx.host.snapshot(&id)).unwrap(),
                serde_json::to_string(&fx.host.get(&id)).unwrap(),
                serde_json::to_string(&fx.host.list(
                    &AnnotationFilter::default(),
                    &ListQuery::default().page().unwrap(),
                ))
                .unwrap(),
                serde_json::to_string(&fx.host.dialogs(&id)).unwrap(),
                format!("{:?}", EngineHost::status(fx.host.as_ref())),
            ];
            primary.handle(1, FrontendFrame::Quit).await.unwrap();
            // Dropped with the instance.
            assert!(fx.host.find(&id).is_none());
            let frames = drain(&mut rx);
            json.extend(frames.iter().map(|f| serde_json::to_string(f).unwrap()));
            (frames, json)
        })
    });
    assert!(!frames.is_empty());
    assert!(
        logs.contains("engine frontend attached"),
        "logging was captured: {logs}"
    );
    assert!(!logs.contains(SECRET), "a log line carried the token");
    for text in &json {
        assert!(!text.contains(SECRET), "leaked in {text}");
    }
    assert!(
        files_containing(&fx.root, SECRET).is_empty(),
        "the token was written to disk"
    );
}

// ─── Host processes ─────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn a_hosted_terminal_release_runs_the_host_process_in_a_hub_pty() {
    use crate::command::commands::workflow_prerequisites::WorkflowPrerequisiteFrontend;
    use crate::command::dispatch::projections::raw_args::ParsedArgs;
    use crate::command::instance::frontend::HostTerminals;
    use crate::engine::auth::host_secret_command::HostSecretOutput;
    use crate::engine::terminal_hub::bridge::HostPty;

    let hub = Arc::new(TerminalHub::new(Default::default()));
    let tab = TabId::new();
    let (frontend, requests, responses) =
        crate::command::instance::test_support::test_command_frontend(
            &["exec", "workflow"],
            ParsedArgs::default(),
        );
    let mut frontend = frontend.with_host_terminals(Some(HostTerminals {
        hub: hub.clone(),
        owner: crate::data::terminal::TerminalOwnerInfo {
            owner: crate::data::terminal::TerminalOwner::Tab(tab.clone()),
            feature: crate::data::daemon_state::FeatureName::Engine,
            source: None,
            attempt: 1,
            allow_remote_input: false,
        },
    }));
    // The frontend side: answer the release, then the restore.
    let answerer = std::thread::spawn(move || {
        let release = requests.recv().unwrap();
        responses.send(DialogResponse::Yes).unwrap();
        let restore = requests.recv().unwrap();
        responses.send(DialogResponse::Dismissed).unwrap();
        (release, restore)
    });
    let mut saw_pty = false;
    let result = frontend.with_terminal_released("Host login", &mut || {
        saw_pty = HostPty::current().is_some();
        Ok(HostSecretOutput {
            secret: None,
            stderr: String::new(),
            extra_lines: 0,
        })
    });
    assert!(result.is_ok());
    assert!(
        saw_pty,
        "the host process ran with the daemon PTY as its terminal"
    );
    assert!(
        HostPty::current().is_none(),
        "the scope ends with the release"
    );
    let (release, restore) = answerer.join().unwrap();
    let DialogRequest::TerminalRelease {
        terminal_id: Some(terminal_id),
        title,
    } = release
    else {
        panic!("a hosted release names its hub terminal, got {release:?}");
    };
    assert_eq!(title, "Host login");
    assert_eq!(restore, DialogRequest::TerminalRestore);
    let record = hub.get(&terminal_id).unwrap();
    assert_eq!(
        record.owner_id,
        crate::data::terminal::TerminalOwner::Tab(tab)
    );
    assert_eq!(record.state, crate::data::terminal::TerminalState::Exited);
}

// ─── Relayed tab state ──────────────────────────────────────────────────────

fn deltas(frames: &[EngineFrame]) -> Vec<&InstanceDelta> {
    frames
        .iter()
        .filter_map(|f| match f {
            EngineFrame::Delta(delta) => Some(delta),
            _ => None,
        })
        .collect()
}

/// Attach the bound command's stuck/status channel to the backbone slot, as
/// the agent runtime does once the container is up.
fn attach_stuck_channel(
    feeds: &crate::command::instance::feeds::TabCommandFeeds,
) -> tokio::sync::broadcast::Sender<crate::engine::agent_runtime::execution::StuckEvent> {
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    feeds.container.push(
        crate::command::instance::feeds::ContainerSlotEvent::EngineAttached {
            launch_id: feeds.container.launch_id(),
            requests: None,
            stuck: Some(rx),
        },
    );
    tx
}

#[tokio::test]
async fn the_agents_dashboard_is_relayed_and_kept_in_the_snapshot() {
    use crate::command::commands::agents::{AgentTotals, AgentsOverview};
    use crate::command::instance::feeds::AgentsDashboardData;
    let fx = fixture();
    let (id, _primary, mut rx) = fx.started().await;
    let tab = fx.first_tab(&id);
    let _command = bind_fake_command(&fx, &id, &tab);
    let feeds = fx.host.test_tab_feeds(&id, &tab);
    drain(&mut rx);

    *feeds.agents_dashboard.lock().unwrap() = Some(AgentsDashboardData {
        overview: AgentsOverview {
            runtime: "docker".into(),
            runtime_display: "Docker".into(),
            available: true,
            unavailable_reason: None,
            agents: Vec::new(),
            totals: AgentTotals::default(),
            collected_at: chrono::Utc::now(),
            tip: String::new(),
        },
        watching: true,
    });
    fx.host.pump_at(Instant::now());
    let frames = drain(&mut rx);
    let relayed = deltas(&frames)
        .into_iter()
        .filter_map(|d| match d {
            InstanceDelta::AgentsDashboard { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(relayed.len(), 1, "one delta for one change");
    assert!(relayed[0].as_ref().is_some_and(|d| d.watching));

    fx.host.pump_at(Instant::now());
    assert!(
        !deltas(&drain(&mut rx))
            .iter()
            .any(|d| matches!(d, InstanceDelta::AgentsDashboard { .. })),
        "an unchanged dashboard is not re-sent"
    );
    let snapshot = fx.host.tab(&id, &tab).unwrap();
    assert!(snapshot.agents_dashboard.is_some_and(|d| d.watching));
}

#[tokio::test]
async fn stuck_and_reported_status_reach_frames_snapshot_and_agents_context() {
    use crate::data::program_status::{EffectiveStatus, ProgramState};
    use crate::engine::agent_runtime::execution::StuckEvent;
    let fx = fixture();
    let (id, _primary, mut rx) = fx.started().await;
    let tab = fx.first_tab(&id);
    let _command = bind_fake_command(&fx, &id, &tab);
    let feeds = fx.host.test_tab_feeds(&id, &tab);
    let stuck = attach_stuck_channel(&feeds);
    fx.host.pump_at(Instant::now());
    drain(&mut rx);

    stuck.send(StuckEvent::Stuck).unwrap();
    fx.host.pump_at(Instant::now());
    assert!(deltas(&drain(&mut rx))
        .iter()
        .any(|d| matches!(d, InstanceDelta::Stuck { stuck: true, .. })));
    assert!(
        fx.host.tab(&id, &tab).unwrap().stuck,
        "a late frontend sees it"
    );
    let context = feeds.tui_context_shared.lock().unwrap().clone();
    assert_eq!(context.tabs.len(), 1);
    assert_eq!(context.tabs[0].tab_number, 1);
    assert!(context.tabs[0].is_stuck);

    let status = EffectiveStatus {
        state: ProgramState::Blocked,
        kind: None,
        progress: None,
        msg: Some("approve?".into()),
        app: None,
        id: None,
    };
    stuck
        .send(StuckEvent::Reported(Some(status.clone())))
        .unwrap();
    fx.host.pump_at(Instant::now());
    let frames = drain(&mut rx);
    assert!(frames.iter().any(|f| matches!(
        f,
        EngineFrame::ProgramStatus { status: Some(s), .. } if s == &status
    )));
    let snapshot = fx.host.tab(&id, &tab).unwrap();
    assert!(!snapshot.stuck, "a reporting agent is never stuck");
    assert_eq!(snapshot.slots[0].program_status.as_ref(), Some(&status));
    assert_eq!(feeds.signals.status(), Some(status.clone()));
    let context = feeds.tui_context_shared.lock().unwrap().clone();
    assert!(!context.tabs[0].is_stuck);
    assert_eq!(context.tabs[0].program_status.as_ref(), Some(&status));
}

#[tokio::test]
async fn a_tui_instance_relays_its_squad_health() {
    let fx = fixture();
    let (_id, _primary, mut rx) = fx.started().await;
    // The probe's first tick runs at once; it finds no squad daemon here.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        fx.host.pump_at(Instant::now());
        let frames = drain(&mut rx);
        let health = deltas(&frames).into_iter().find_map(|d| match d {
            InstanceDelta::SquadHealth(health) => Some(*health),
            _ => None,
        });
        if let Some(health) = health.filter(|h| *h != SquadHealthWire::Unknown) {
            assert_ne!(health, SquadHealthWire::Running);
            assert_ne!(health, SquadHealthWire::Healthy);
            break;
        }
        assert!(Instant::now() < deadline, "no squad health was relayed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
