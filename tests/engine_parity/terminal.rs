use super::multi_frontend::attach;
use crate::{
    command::integrations::IntegrationRegistry,
    data::{
        annotations::Annotations, daemon_state::FeatureName, frontend_protocol::FrontendRole,
        terminal::*,
    },
    engine::{
        remote::terminal_client::{TerminalClientOptions, TerminalStream},
        terminal_hub::{input_channel, TerminalHub, TerminalInput, TerminalRegistration},
    },
    frontend::terminal_output_filter::HostOutputFilter,
    support::Daemon,
};
use std::{sync::Arc, time::Duration};
async fn frame(stream: &mut TerminalStream) -> TerminalFrame {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_reconnect_snapshot_resize_input_owner_filter_and_exit_match_builtin() {
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let id = d.host().create_instance(Annotations::new()).unwrap();
    let (_tx, _rx) = attach(&d, &id, d.hello(FrontendRole::Primary)).await;
    let tab = d.host().tabs(&id).unwrap()[0].tab_id.clone();
    let owner = TerminalOwnerInfo {
        owner: TerminalOwner::Tab(tab),
        feature: FeatureName::Engine,
        source: None,
        attempt: 1,
        allow_remote_input: false,
    };
    let registration = TerminalRegistration {
        owner,
        agent: "fake".into(),
        model: None,
        cols: 80,
        rows: 24,
    };
    let (input, mut input_rx) = input_channel();
    let (sizes, mut sizes_rx) = tokio::sync::mpsc::unbounded_channel();
    let remote = d.api.terminals.hub().register(
        registration.clone(),
        TerminalInput {
            stdin: input,
            resize: Some(Arc::new(move |c, r| {
                sizes.send((c, r)).unwrap();
            })),
        },
    );
    let builtin = Arc::new(TerminalHub::new(Default::default()));
    let (bi, mut bi_rx) = input_channel();
    let local = builtin.register(
        registration,
        TerminalInput {
            stdin: bi,
            resize: None,
        },
    );
    let mut direct = builtin.subscribe(local.id()).unwrap();
    let options = TerminalClientOptions::default();
    let mut wire = TerminalStream::connect_local(&d.socket, remote.id(), &options)
        .await
        .unwrap();
    assert_eq!(frame(&mut wire).await, direct.next().await.unwrap());
    let bytes = b"prompt\r\n\x1b[31mred\x1b[0m\x1b]52;c;YXR0YWNr\x07\x1b]0;safe title\x07";
    remote.push_output(bytes);
    local.push_output(bytes);
    let observed = frame(&mut wire).await;
    assert_eq!(observed, direct.next().await.unwrap());
    let TerminalFrame::Output { bytes: streamed } = observed else {
        panic!("output")
    };
    let mut remote_filter = HostOutputFilter::new(Default::default());
    let mut local_filter = HostOutputFilter::new(Default::default());
    let mut r = vec![];
    let mut l = vec![];
    for chunk in streamed.chunks(3) {
        remote_filter.feed(chunk, &mut r);
    }
    for chunk in bytes.chunks(7) {
        local_filter.feed(chunk, &mut l);
    }
    remote_filter.finish(&mut r);
    local_filter.finish(&mut l);
    assert_eq!(r, l);
    assert!(!r.windows(4).any(|w| w == b"]52;"));
    wire.send(TerminalClientFrame::TakeControl).await.unwrap();
    wire.send(TerminalClientFrame::Input {
        bytes: b"answer\n".to_vec(),
    })
    .await
    .unwrap();
    let grant = builtin.take_control(local.id()).unwrap();
    grant.send_input(b"answer\n".to_vec()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), input_rx.recv())
            .await
            .unwrap(),
        bi_rx.recv().await
    );
    // A second frontend cannot steal control or resize the PTY.
    let second = d
        .api
        .terminals
        .open(
            remote.id(),
            ViewerIdentity {
                listener: crate::data::daemon_state::CommandListener::Local,
                actor: None,
                annotations: Default::default(),
                read_only: false,
            },
        )
        .unwrap();
    let mut second = second;
    assert!(second
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .is_err());
    assert!(second
        .handle_client_frame(TerminalClientFrame::Input {
            bytes: b"stolen".to_vec()
        })
        .is_err());
    assert!(input_rx.try_recv().is_err());
    wire.send(TerminalClientFrame::Resize {
        cols: 101,
        rows: 33,
    })
    .await
    .unwrap();
    grant.resize(101, 33).unwrap();
    assert_eq!(frame(&mut wire).await, direct.next().await.unwrap());
    assert_eq!(sizes_rx.recv().await, Some((101, 33)));
    wire.close().await.unwrap();
    grant.release();
    remote.push_output(b"\r\nwhile disconnected");
    local.push_output(b"\r\nwhile disconnected");
    let mut wire = TerminalStream::connect_local(&d.socket, remote.id(), &options)
        .await
        .unwrap();
    let mut direct = builtin.subscribe(local.id()).unwrap();
    assert_eq!(frame(&mut wire).await, direct.next().await.unwrap());
    remote.exit(37);
    local.exit(37);
    assert_eq!(frame(&mut wire).await, direct.next().await.unwrap());
    assert!(matches!(
        d.api.terminals.hub().get(remote.id()).unwrap().state,
        TerminalState::Exited
    ));
}
