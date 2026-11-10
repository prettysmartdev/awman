use super::*;
use crate::data::daemon_state::FeatureName;
use crate::data::execution_event::EventSourcePhase;
use crate::data::ids::{CommandId, TabId};
use crate::data::terminal::{Annotations, TerminalOwnerInfo};
use crate::engine::terminal_hub::{TerminalHubConfig, TerminalInput, TerminalRegistration};

#[derive(Default)]
struct Log(Mutex<Vec<(TerminalOwner, Option<EventSource>, EventPayload)>>);

impl OwnerEventLog for Log {
    fn append(&self, owner: &TerminalOwner, payload: EventPayload) {
        self.0.lock().unwrap().push((owner.clone(), None, payload));
    }
    fn append_with_source(
        &self,
        owner: &TerminalOwner,
        source: Option<&EventSource>,
        payload: EventPayload,
    ) {
        self.0
            .lock()
            .unwrap()
            .push((owner.clone(), source.cloned(), payload));
    }
}

impl Log {
    fn payloads(&self) -> Vec<EventPayload> {
        self.0.lock().unwrap().iter().map(|e| e.2.clone()).collect()
    }
}

struct Runs(Option<RunId>);
impl OwnerResolver for Runs {
    fn active_run(&self, _task: &TaskId) -> Option<RunId> {
        self.0.clone()
    }
}

struct Fixture {
    ops: Arc<TerminalOperations>,
    log: Arc<Log>,
    policy: Arc<Mutex<AccessPolicy>>,
}

fn fixture_with(run: Option<RunId>) -> Fixture {
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
    let log = Arc::new(Log::default());
    let policy = Arc::new(Mutex::new(AccessPolicy::default()));
    let read = policy.clone();
    let ops = Arc::new(TerminalOperations::new(
        hub,
        Arc::new(move || read.lock().unwrap().clone()),
        log.clone(),
        Arc::new(Runs(run)),
    ));
    Fixture { ops, log, policy }
}

fn fixture() -> Fixture {
    fixture_with(None)
}

fn register(
    ops: &TerminalOperations,
    owner: TerminalOwner,
    step: Option<&str>,
    allow_remote_input: bool,
) -> (
    crate::engine::terminal_hub::TerminalHandle,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (stdin, rx) = crate::engine::terminal_hub::input_channel();
    let handle = ops.hub().register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner,
                feature: FeatureName::Worker,
                source: step.map(|s| EventSource::step(EventSourcePhase::Main, s, 0)),
                attempt: 1,
                allow_remote_input,
            },
            agent: "claude".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    (handle, rx)
}

fn network(actor: Option<&str>) -> ViewerIdentity {
    ViewerIdentity {
        principal: crate::data::daemon_state::Principal::Owner,
        listener_name: Some("default".into()),
        listener: CommandListener::Network,
        actor: actor.map(str::to_owned),
        annotations: Annotations::try_from_pairs([("rex/block", "b1")]).unwrap(),
        read_only: false,
    }
}

#[test]
fn list_filters_by_owner_and_hides_unserved_terminals_from_the_network() {
    let f = fixture();
    let cmd = TerminalOwner::Command(CommandId::new());
    let tab = TerminalOwner::Tab(TabId::new());
    let (a, _) = register(&f.ops, cmd.clone(), None, false);
    let (_b, _) = register(&f.ops, tab.clone(), None, false);
    let local = ViewerIdentity::local();
    let all = f.ops.list(None, &ListQuery::default(), &local).unwrap();
    assert_eq!(all.items.len(), 2);
    let mine = f
        .ops
        .list(Some(&cmd), &ListQuery::default(), &local)
        .unwrap();
    assert_eq!(mine.items.len(), 1);
    assert_eq!(&mine.items[0].id, a.id());
    assert!(mine.items[0].id.as_str().starts_with("term_"));

    f.policy.lock().unwrap().network_features =
        [("default".into(), vec![FeatureName::Worker])].into();
    let net = f
        .ops
        .list(None, &ListQuery::default(), &network(None))
        .unwrap();
    assert_eq!(net.items.len(), 1);
    assert_eq!(net.items[0].owner_id, cmd);
}

#[test]
fn unknown_and_forgotten_ids_are_gone() {
    let f = fixture();
    let local = ViewerIdentity::local();
    assert_eq!(
        f.ops.get(&TerminalId::new(), &local),
        Err(TerminalRefusal::Gone)
    );
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, _) = register(&f.ops, owner, None, false);
    handle.exit(0);
    assert!(f.ops.hub().forget(handle.id()));
    assert_eq!(f.ops.get(handle.id(), &local), Err(TerminalRefusal::Gone));
}

#[test]
fn resolve_target_picks_the_open_terminal_and_honours_step() {
    let run = RunId::new();
    let f = fixture_with(Some(run.clone()));
    let cmd = CommandId::new();
    let owner = TerminalOwner::Command(cmd.clone());
    let (first, _) = register(&f.ops, owner.clone(), Some("plan"), false);
    let (second, _) = register(&f.ops, owner.clone(), Some("build"), false);
    let target = AttachTarget::Command(cmd.clone());
    assert_eq!(&f.ops.resolve_target(&target, None).unwrap(), second.id());
    assert_eq!(
        &f.ops.resolve_target(&target, Some("plan")).unwrap(),
        first.id()
    );
    second.exit(0);
    assert_eq!(&f.ops.resolve_target(&target, None).unwrap(), first.id());
    assert!(matches!(
        f.ops.resolve_target(&target, Some("review")),
        Err(TerminalRefusal::NoTerminal(_))
    ));
    assert_eq!(
        &f.ops
            .resolve_target(&AttachTarget::Terminal(first.id().clone()), None)
            .unwrap(),
        first.id()
    );
    let (task_term, _) = register(&f.ops, TerminalOwner::Run(run), None, false);
    assert_eq!(
        &f.ops
            .resolve_target(&AttachTarget::Task(TaskId::new()), None)
            .unwrap(),
        task_term.id()
    );
}

#[test]
fn input_is_refused_without_both_settings() {
    let f = fixture();
    f.policy.lock().unwrap().network_features =
        [("default".into(), vec![FeatureName::Worker])].into();
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, _rx) = register(&f.ops, owner.clone(), None, true);
    let mut session = f.ops.open(handle.id(), network(Some("webapp"))).unwrap();
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::TakeControl),
        Err(TerminalRefusal::InputNotAllowed)
    );
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::Input {
            bytes: b"x".to_vec()
        }),
        Err(TerminalRefusal::InputNotAllowed)
    );
    // remoteInput on, but the command did not allow remote input.
    f.policy.lock().unwrap().remote_input = true;
    let (closed, _rx2) = register(&f.ops, owner, None, false);
    let mut other = f.ops.open(closed.id(), network(Some("webapp"))).unwrap();
    assert_eq!(
        other.handle_client_frame(TerminalClientFrame::TakeControl),
        Err(TerminalRefusal::InputNotAllowed)
    );
    assert!(f.log.payloads().is_empty());
}

#[test]
fn take_control_is_exclusive_and_logged() {
    let f = fixture();
    {
        let mut p = f.policy.lock().unwrap();
        p.network_features = [("default".into(), vec![FeatureName::Worker])].into();
        p.remote_input = true;
    }
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, mut stdin) = register(&f.ops, owner.clone(), Some("build"), true);
    let mut first = f.ops.open(handle.id(), network(Some("alice"))).unwrap();
    let mut second = f.ops.open(handle.id(), network(Some("bob"))).unwrap();
    first
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    assert!(first.holds_grant());
    assert_eq!(
        second.handle_client_frame(TerminalClientFrame::TakeControl),
        Err(TerminalRefusal::ControlHeld)
    );
    assert_eq!(
        f.ops.control_holder(handle.id()),
        Some(ControlHolder::Actor("alice".into()))
    );
    first
        .handle_client_frame(TerminalClientFrame::Input {
            bytes: b"ls\r".to_vec(),
        })
        .unwrap();
    assert_eq!(stdin.try_recv().unwrap(), b"ls\r");
    assert_eq!(
        second.handle_client_frame(TerminalClientFrame::Input {
            bytes: b"x".to_vec()
        }),
        Err(TerminalRefusal::InputNotAllowed)
    );

    first.detach(DetachReason::User);
    second
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();

    let log = f.log.0.lock().unwrap().clone();
    let holders: Vec<_> = log
        .iter()
        .filter_map(|(o, source, p)| match p {
            EventPayload::TerminalControl {
                holder,
                annotations,
                terminal_id,
            } => {
                assert_eq!(o, &owner);
                assert_eq!(terminal_id, handle.id());
                assert_eq!(annotations.get("rex/block").unwrap(), "b1");
                assert_eq!(source.as_ref().unwrap().step_name.as_deref(), Some("build"));
                Some(holder.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(holders, ["alice", "", "bob"]);
    // Network viewers do not produce `terminal_viewer_detached`.
    assert!(!log
        .iter()
        .any(|e| matches!(e.2, EventPayload::TerminalViewerDetached { .. })));
}

#[test]
fn local_detach_logs_the_declared_reason_with_annotations() {
    let f = fixture();
    let owner = TerminalOwner::Tab(TabId::new());
    let (handle, _rx) = register(&f.ops, owner, None, false);
    let mut viewer = ViewerIdentity::local();
    viewer.annotations.insert("rex/block", "blk_9").unwrap();
    let mut session = f.ops.open(handle.id(), viewer.clone()).unwrap();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    session
        .handle_client_frame(TerminalClientFrame::Detached {
            reason: DetachReason::Hangup,
        })
        .unwrap();
    session.detach(DetachReason::User);
    // Dropped without detach: a hang-up.
    drop(f.ops.open(handle.id(), viewer).unwrap());
    let payloads = f.log.payloads();
    let detached: Vec<_> = payloads
        .iter()
        .filter_map(|p| match p {
            EventPayload::TerminalViewerDetached {
                reason,
                annotations,
                ..
            } => Some((*reason, annotations.get("rex/block").unwrap().to_owned())),
            _ => None,
        })
        .collect();
    assert_eq!(
        detached,
        [
            (DetachReason::Hangup, "blk_9".to_owned()),
            (DetachReason::Hangup, "blk_9".to_owned())
        ]
    );
    let controls = payloads
        .iter()
        .filter(|p| matches!(p, EventPayload::TerminalControl { holder, .. } if holder == "local" || holder.is_empty()))
        .count();
    assert_eq!(controls, 2);
}

#[test]
fn read_only_viewers_never_resize_or_take_control() {
    let f = fixture();
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, _rx) = register(&f.ops, owner, None, false);
    let mut viewer = ViewerIdentity::local();
    viewer.read_only = true;
    let mut session = f.ops.open(handle.id(), viewer).unwrap();
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::TakeControl),
        Err(TerminalRefusal::InputNotAllowed)
    );
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::Resize { cols: 10, rows: 5 }),
        Err(TerminalRefusal::InputNotAllowed)
    );
    assert_eq!(
        f.ops
            .get(handle.id(), &ViewerIdentity::local())
            .unwrap()
            .cols,
        80
    );
}

#[tokio::test]
async fn lifecycle_events_reach_the_owner_log_with_the_source() {
    let f = fixture();
    let publisher = f.ops.spawn_lifecycle_publisher();
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, _rx) = register(&f.ops, owner.clone(), Some("build"), false);
    handle.exit(3);
    for _ in 0..100 {
        if f.log.payloads().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    publisher.abort();
    let log = f.log.0.lock().unwrap().clone();
    assert!(matches!(
        &log[0].2,
        EventPayload::TerminalOpened { terminal_id, cols: 80, rows: 24, .. } if terminal_id == handle.id()
    ));
    assert!(matches!(
        &log[1].2,
        EventPayload::TerminalClosed { terminal_id, exit_code: 3 } if terminal_id == handle.id()
    ));
    for (o, source, _) in &log {
        assert_eq!(o, &owner);
        assert_eq!(source.as_ref().unwrap().step_name.as_deref(), Some("build"));
    }
}

/// Review finding: a grant taken while network input was allowed must stop
/// working as soon as the live policy revokes it — for input, resize and a
/// repeated `take_control` — without the viewer reconnecting.
#[test]
fn revoked_remote_input_stops_an_existing_holder() {
    let f = fixture();
    {
        let mut policy = f.policy.lock().unwrap();
        policy.network_features = [("default".into(), vec![FeatureName::Worker])].into();
        policy.remote_input = true;
    }
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, mut rx) = register(&f.ops, owner, None, true);
    let mut session = f.ops.open(handle.id(), network(Some("review"))).unwrap();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    session
        .handle_client_frame(TerminalClientFrame::Input {
            bytes: b"ok".to_vec(),
        })
        .unwrap();
    assert_eq!(rx.try_recv().unwrap(), b"ok");

    f.policy.lock().unwrap().remote_input = false;
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::Input {
            bytes: b"whoami\r".to_vec()
        }),
        Err(TerminalRefusal::InputNotAllowed)
    );
    assert!(rx.try_recv().is_err(), "revoked input reached the PTY");
    assert!(!session.holds_grant(), "revoked grant must be released");
    assert!(f.ops.control_holder(handle.id()).is_none());
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::Resize { cols: 90, rows: 30 }),
        Err(TerminalRefusal::InputNotAllowed)
    );
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::TakeControl),
        Err(TerminalRefusal::InputNotAllowed)
    );

    // Restored policy: a fresh take works; then un-serving the feature
    // revokes a held grant on a repeated take_control too.
    f.policy.lock().unwrap().remote_input = true;
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    f.policy.lock().unwrap().network_features.clear();
    assert_eq!(
        session.handle_client_frame(TerminalClientFrame::TakeControl),
        Err(TerminalRefusal::InputNotAllowed)
    );
    assert!(!session.holds_grant());
}

/// Review finding: an output subscription is re-admitted under the live
/// policy; un-serving the owner's feature on the network ends it.
#[tokio::test]
async fn network_output_stops_when_the_feature_is_no_longer_served() {
    let f = fixture();
    f.policy.lock().unwrap().network_features =
        [("default".into(), vec![FeatureName::Worker])].into();
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, _rx) = register(&f.ops, owner, None, false);
    let mut session = f.ops.open(handle.id(), network(Some("review"))).unwrap();
    assert!(session.next_frame().await.is_some(), "snapshot first");
    f.policy.lock().unwrap().network_features.clear();
    handle.push_output(b"secret");
    assert_eq!(session.next_frame().await, None);
    assert!(session.access_revoked());
    // Local viewers are unaffected by the network listing.
    let mut local = f.ops.open(handle.id(), ViewerIdentity::local()).unwrap();
    assert!(local.next_frame().await.is_some());
    assert!(!local.access_revoked());
}

/// Review finding: a stalled agent must push back on viewer input instead
/// of queueing it without bound.
#[test]
fn stalled_stdin_backpressures_the_holder() {
    let f = fixture();
    let owner = TerminalOwner::Command(CommandId::new());
    let (handle, mut rx) = register(&f.ops, owner, None, false);
    let mut session = f.ops.open(handle.id(), ViewerIdentity::local()).unwrap();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    let frame = || TerminalClientFrame::Input {
        bytes: vec![b'x'; 128 * 1024],
    };
    let mut accepted = 0;
    let mut refused = None;
    for _ in 0..128 {
        match session.handle_client_frame(frame()) {
            Ok(()) => accepted += 1,
            Err(refusal) => {
                refused = Some(refusal);
                break;
            }
        }
    }
    assert_eq!(refused, Some(TerminalRefusal::InputBackpressure));
    assert!(accepted < 128);
    let queued: usize = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|c| c.len())
        .sum();
    assert!(
        queued
            <= crate::engine::terminal_hub::INPUT_QUEUE_CHUNKS
                * crate::engine::terminal_hub::MAX_INPUT_CHUNK_BYTES,
        "{queued} bytes queued"
    );
    // Drained: the holder kept its grant and input flows again.
    assert!(session.holds_grant());
    session.handle_client_frame(frame()).unwrap();
}

#[tokio::test]
async fn lifecycle_audit_does_not_lose_a_burst_larger_than_the_broadcast_ring() {
    let f = fixture();
    let publisher = f.ops.spawn_lifecycle_publisher();
    for _ in 0..512 {
        let (terminal, _input) = register(
            &f.ops,
            TerminalOwner::Command(crate::data::ids::CommandId::new()),
            None,
            false,
        );
        terminal.exit(0);
        f.ops.hub().forget(terminal.id());
    }
    let payloads = f.log.payloads();
    assert_eq!(
        payloads
            .iter()
            .filter(|p| matches!(p, EventPayload::TerminalOpened { .. }))
            .count(),
        512
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|p| matches!(p, EventPayload::TerminalClosed { .. }))
            .count(),
        512
    );
    publisher.abort();
}

#[tokio::test]
async fn exited_terminal_revokes_and_logs_the_viewers_grant() {
    let f = fixture();
    let (terminal, _input) = register(
        &f.ops,
        TerminalOwner::Command(crate::data::ids::CommandId::new()),
        None,
        false,
    );
    let mut viewer = f.ops.open(terminal.id(), ViewerIdentity::local()).unwrap();
    viewer
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    assert!(viewer.holds_grant());
    terminal.exit(7);
    assert!(!viewer.holds_grant());
    assert!(matches!(
        viewer.next_frame().await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    assert!(matches!(
        viewer.next_frame().await,
        Some(TerminalFrame::Exit { code: 7 })
    ));
    assert!(f
        .log
        .payloads()
        .iter()
        .any(|p| matches!(p, EventPayload::TerminalControl { holder, .. } if holder.is_empty())));
}

#[test]
fn contending_control_events_alternate_acquire_and_release() {
    let f = fixture();
    let (terminal, _input) = register(
        &f.ops,
        TerminalOwner::Command(crate::data::ids::CommandId::new()),
        None,
        false,
    );
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let ops = f.ops.clone();
            let id = terminal.id().clone();
            scope.spawn(move || {
                for _ in 0..300 {
                    let mut viewer = ops.open(&id, ViewerIdentity::local()).unwrap();
                    if viewer
                        .handle_client_frame(TerminalClientFrame::TakeControl)
                        .is_ok()
                    {
                        viewer.detach(DetachReason::User);
                    }
                }
            });
        }
    });
    let holders: Vec<_> = f
        .log
        .payloads()
        .into_iter()
        .filter_map(|payload| match payload {
            EventPayload::TerminalControl { holder, .. } => Some(holder),
            _ => None,
        })
        .collect();
    assert!(!holders.is_empty());
    assert_eq!(holders.len() % 2, 0);
    for pair in holders.chunks_exact(2) {
        assert!(!pair[0].is_empty());
        assert!(pair[1].is_empty());
    }
}
