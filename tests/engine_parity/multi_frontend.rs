use crate::{
    command::{
        dispatch::LaunchDisplay,
        instance::{feeds::ContainerSlotIo, SubmittedCommand},
        integrations::{HostPresenter, HostRequirement, Integration, IntegrationRegistry},
    },
    data::{
        annotations::Annotations,
        config::{env::EnvSnapshot, EffectiveConfig},
        dialog::{DialogRequest, DialogResponse},
        frontend_protocol::*,
        ids::{FrontendId, InstanceId},
    },
    engine::remote::frontend_client::{FrontendReceiver, FrontendSender, FrontendStream},
    support::Daemon,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub async fn next(rx: &mut FrontendReceiver) -> EngineFrame {
    tokio::time::timeout(Duration::from_secs(5), rx.next())
        .await
        .expect("frontend deadline")
        .unwrap()
        .expect("connected frontend")
        .1
}
pub async fn until(
    rx: &mut FrontendReceiver,
    predicate: impl Fn(&EngineFrame) -> bool,
) -> EngineFrame {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = next(rx).await;
            if predicate(&frame) {
                return frame;
            }
            seen.push(format!("{frame:?}"));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected engine frame; observed: {seen:?}"))
}
pub async fn attach(
    d: &Daemon,
    id: &InstanceId,
    hello: Hello,
) -> (FrontendSender, FrontendReceiver) {
    let (mut tx, mut rx) = FrontendStream::connect_local(&d.socket, id)
        .await
        .unwrap()
        .split();
    tx.send(&FrontendFrame::Hello(hello)).await.unwrap();
    assert!(matches!(next(&mut rx).await, EngineFrame::Welcome { .. }));
    until(&mut rx, |f| matches!(f, EngineFrame::Snapshot(_))).await;
    (tx, rx)
}
pub struct FakeCommand {
    pub wake: Arc<tokio::sync::Notify>,
    pub requests: std::sync::mpsc::Sender<DialogRequest>,
    pub responses: std::sync::mpsc::Receiver<DialogResponse>,
    pub _result: std::sync::mpsc::Sender<
        Result<crate::command::CommandOutcome, crate::command::CommandError>,
    >,
    pub _stdout: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}
pub fn bind(d: &Daemon, id: &InstanceId) -> FakeCommand {
    let tab = d.host().tabs(id).unwrap()[0].tab_id.clone();
    let (requests, rx) = std::sync::mpsc::channel();
    let (tx, responses) = std::sync::mpsc::channel();
    let (result, rrx) = std::sync::mpsc::channel();
    let (stdout, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let wake = d.host().bind_test_command(
        id,
        &tab,
        SubmittedCommand {
            display: LaunchDisplay {
                agent_display_name: "fake".into(),
                unattended: false,
            },
            sandboxed: false,
            feeds: crate::command::instance::feeds::TabFeeds {
                dialog_request_rx: rx,
                dialog_response_tx: tx,
                command_result_rx: rrx,
            },
            slot_io: ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        },
    );
    FakeCommand {
        wake,
        requests,
        responses,
        _result: result,
        _stdout: stdout,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_frontends_receive_content_first_answer_wins_and_losers_close() {
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let id = d.host().create_instance(Annotations::new()).unwrap();
    let (mut p, mut pr) = attach(&d, &id, d.hello(FrontendRole::Primary)).await;
    let (mut a, mut ar) = attach(&d, &id, d.hello(FrontendRole::Secondary)).await;
    let (_, mut br) = attach(&d, &id, d.hello(FrontendRole::Secondary)).await;
    let fake = bind(&d, &id);
    let request = DialogRequest::YesNo {
        title: "Conformance".into(),
        body: "Identical content\nsecond line".into(),
    };
    fake.requests.send(request.clone()).unwrap();
    let mut dialog = None;
    for rx in [&mut pr, &mut ar, &mut br] {
        let EngineFrame::DialogRequest {
            dialog_id,
            request: received,
            ..
        } = until(rx, |f| matches!(f, EngineFrame::DialogRequest { .. })).await
        else {
            unreachable!()
        };
        assert_eq!(received, request);
        if let Some(expected) = &dialog {
            assert_eq!(expected, &dialog_id);
        } else {
            dialog = Some(dialog_id);
        }
    }
    let dialog = dialog.unwrap();
    a.send(&FrontendFrame::DialogResponse {
        dialog_id: dialog.clone(),
        response: DialogResponse::Yes,
    })
    .await
    .unwrap();
    let answer = tokio::task::spawn_blocking(move || {
        fake.responses.recv_timeout(Duration::from_secs(5)).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(answer, DialogResponse::Yes);
    for rx in [&mut pr, &mut br] {
        assert!(
            matches!(until(rx,|f|matches!(f,EngineFrame::DialogClosed{..})).await,EngineFrame::DialogClosed{dialog_id,..} if dialog_id==dialog)
        );
    }
    p.send(&FrontendFrame::DialogResponse {
        dialog_id: dialog.clone(),
        response: DialogResponse::No,
    })
    .await
    .unwrap();
    assert!(matches!(
        d.host()
            .answer(&id, &dialog, DialogResponse::No, AnsweredBy::Api)
            .unwrap(),
        crate::data::instance_snapshot::DialogAnswerOutcome::AlreadyAnswered { .. }
    ));
    p.send(&FrontendFrame::Quit).await.unwrap();
}

#[derive(Default)]
pub struct Presenter(pub Mutex<Vec<UiRequest>>);
impl HostPresenter for Presenter {
    fn bind(&self, _: crate::command::integrations::PresenterHandle) {}
    fn on_instance_event(&self, _: &InstanceId, _: &crate::command::integrations::PresenterEvent) {}
    fn name(&self) -> &'static str {
        "parity"
    }
    fn on_ui_request(&self, _: &InstanceId, _: &FrontendId, request: &UiRequest) -> UiOutcome {
        self.0.lock().unwrap().push(request.clone());
        UiOutcome::claimed("parity")
    }
}
struct Plugin(Arc<Presenter>);
impl Integration for Plugin {
    fn name(&self) -> &'static str {
        "parity"
    }
    fn detect(&self, _: &EnvSnapshot, _: &EffectiveConfig) -> HostRequirement {
        HostRequirement::None
    }
    fn presenter(&self) -> Option<Arc<dyn HostPresenter>> {
        Some(self.0.clone())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ui_request_claim_is_delivered_over_the_production_socket() {
    let presenter = Arc::new(Presenter::default());
    let registry = IntegrationRegistry::with(vec![Arc::new(Plugin(presenter.clone()))]);
    let d = Daemon::start(registry).await;
    let id = d.host().create_instance(Annotations::new()).unwrap();
    let mut hello = d.hello(FrontendRole::Primary);
    hello.annotations.insert("parity/window", "one").unwrap();
    let (mut tx, mut rx) = attach(&d, &id, hello).await;
    let seq = tx
        .send(&FrontendFrame::UiRequest(UiRequest::NextTab))
        .await
        .unwrap();
    assert!(
        matches!(until(&mut rx,|f|matches!(f,EngineFrame::UiAck{..})).await,EngineFrame::UiAck{request_seq,outcome} if request_seq==seq && outcome.claimed_by_host && outcome.claimed_by.as_deref()==Some("parity"))
    );
    assert_eq!(*presenter.0.lock().unwrap(), vec![UiRequest::NextTab]);
}
