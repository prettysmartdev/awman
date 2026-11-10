use super::multi_frontend::{attach, bind, until};
use crate::{
    command::integrations::IntegrationRegistry,
    data::{
        annotations::Annotations,
        dialog::{DialogRequest, DialogResponse},
        frontend_protocol::*,
    },
    support::Daemon,
};
use std::time::{Duration, Instant};
fn budget(name: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    eprintln!("engine parity {name}: n={} p95={p95:?}", samples.len());
    if std::env::var("AWMAN_TEST_SLOW_HOST").as_deref() == Ok("1") {
        eprintln!("explicit AWMAN_TEST_SLOW_HOST=1: strict latency budget disabled");
    } else {
        assert!(p95<Duration::from_millis(5),"{name} p95 {p95:?} must be <5 ms; see tests/engine_parity/README.md for explicit slow-host opt-out");
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unix_socket_submit_and_dialog_p95_are_under_five_ms() {
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let id = d.host().create_instance(Annotations::new()).unwrap();
    let (mut tx, mut rx) = attach(&d, &id, d.hello(FrontendRole::Primary)).await;
    let tab = d.host().tabs(&id).unwrap()[0].tab_id.clone();
    let mut submit = vec![];
    for i in 0..325 {
        let start = Instant::now();
        tx.send(&FrontendFrame::Submit {
            tab_id: tab.clone(),
            input: SubmitInput::CommandBox {
                raw: "new spec".into(),
            },
            pty_size: TermSize {
                cols: 100,
                rows: 30,
            },
        })
        .await
        .unwrap();
        until(&mut rx, |f| matches!(f, EngineFrame::CommandOutput { .. })).await;
        let elapsed = start.elapsed();
        loop {
            match until(&mut rx, |f| {
                matches!(
                    f,
                    EngineFrame::DialogRequest { .. } | EngineFrame::CommandFinished { .. }
                )
            })
            .await
            {
                EngineFrame::DialogRequest {
                    dialog_id, request, ..
                } => {
                    let response = match request {
                        DialogRequest::KindSelect { .. } => DialogResponse::Index(0),
                        _ => DialogResponse::Text("latency fixture".into()),
                    };
                    tx.send(&FrontendFrame::DialogResponse {
                        dialog_id,
                        response,
                    })
                    .await
                    .unwrap();
                }
                EngineFrame::CommandFinished { .. } => break,
                _ => unreachable!(),
            }
        }
        if i >= 25 {
            submit.push(elapsed);
        }
    }
    let fake = bind(&d, &id);
    let mut dialogs = vec![];
    for i in 0..325 {
        let start = Instant::now();
        fake.requests
            .send(DialogRequest::YesNo {
                title: "Latency".into(),
                body: "Answer?".into(),
            })
            .unwrap();
        // Match InstanceFrontend::ask_dialog: publishing a request wakes the
        // host. Include request delivery and answer acceptance in the timer.
        fake.wake.notify_one();
        let EngineFrame::DialogRequest { dialog_id, .. } =
            until(&mut rx, |f| matches!(f, EngineFrame::DialogRequest { .. })).await
        else {
            unreachable!()
        };
        tx.send(&FrontendFrame::DialogResponse {
            dialog_id,
            response: DialogResponse::Yes,
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(answer) = fake.responses.try_recv() {
                    assert_eq!(answer, DialogResponse::Yes);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if i >= 25 {
            dialogs.push(start.elapsed());
        }
    }
    budget("Submit -> first CommandOutput", submit);
    budget("DialogRequest -> DialogResponse", dialogs);
}

/// Also include the actual backend's std-channel adapter. A fast raw socket
/// must not hide a slow presenter-answer polling interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_backend_channel_adapter_obeys_the_same_latency_budget() {
    use crate::command::instance::{
        backend::InstanceBackend, feeds::TabCommandFeeds, InitialTabOpened, InitialTabSpec,
        SubmitRequest,
    };
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let remote = d.remote().await;
    let session = crate::data::session::Session::open_at_git_root(
        d.root.join("repo"),
        d.root.join("repo"),
        crate::data::session::SessionOpenOptions {
            env: Some(d.api.runtime.core().env().clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let InitialTabOpened::Normal { tab } = remote.open_initial(InitialTabSpec::Session {
        session,
        fatal_startup_error: true,
        pty_size: (100, 30),
    }) else {
        panic!("normal tab")
    };
    let session_id = tab.session.id();
    let stats = tokio::task::spawn_blocking(move || {
        for _ in 0..325 {
            let command = remote
                .submit(SubmitRequest {
                    lease_id: None,
                    session_id,
                    parsed: crate::command::dispatch::parsed_input::parse(
                        "new spec",
                        crate::command::dispatch::catalogue::CommandCatalogue::get(),
                    )
                    .unwrap(),
                    feeds: TabCommandFeeds::new(),
                    pty_size: (100, 30),
                    squad_attach: None,
                })
                .unwrap();
            for _ in 0..3 {
                let request = command
                    .feeds
                    .dialog_request_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                let answer = if matches!(request, DialogRequest::KindSelect { .. }) {
                    DialogResponse::Index(0)
                } else {
                    DialogResponse::Text("backend latency".into())
                };
                command.feeds.dialog_response_tx.send(answer).unwrap();
            }
            assert!(command
                .feeds
                .command_result_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_ok());
        }
        let stats = remote.latency_probe();
        remote.quit();
        stats
    })
    .await
    .unwrap();
    assert_eq!(stats.submit_samples, 325);
    assert_eq!(stats.dialog_samples, 975);
    for (name, value) in [
        ("backend Submit", stats.submit_to_first_output_p95.unwrap()),
        (
            "backend DialogResponse",
            stats.dialog_round_trip_p95.unwrap(),
        ),
    ] {
        eprintln!("engine parity {name}: p95={value:?}");
        if std::env::var("AWMAN_TEST_SLOW_HOST").as_deref() != Ok("1") {
            assert!(
                value < Duration::from_millis(5),
                "{name} p95={value:?} exceeds 5ms"
            );
        }
    }
}

/// Baseline for the delivered owner-socket terminal path. This measures the
/// real hub/codec/router with synthetic output; it is not browser rendering or
/// an agent PTY benchmark. Browser baselines must be measured separately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_frame_throughput_and_reconnect_baseline() {
    use crate::{
        data::{daemon_state::FeatureName, terminal::*},
        engine::{
            remote::terminal_client::{TerminalClientOptions, TerminalStream},
            terminal_hub::{input_channel, TerminalInput, TerminalRegistration},
        },
    };
    async fn next(wire: &mut TerminalStream) -> TerminalFrame {
        tokio::time::timeout(Duration::from_secs(5), wire.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }
    fn p95(samples: &mut [Duration]) -> f64 {
        samples.sort();
        samples[(samples.len() * 95).div_ceil(100) - 1].as_secs_f64() * 1000.0
    }
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let id = d.host().create_instance(Annotations::new()).unwrap();
    let (_tx, _rx) = attach(&d, &id, d.hello(FrontendRole::Primary)).await;
    let tab = d.host().tabs(&id).unwrap()[0].tab_id.clone();
    let (stdin, _input) = input_channel();
    let terminal = d.api.terminals.hub().register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Tab(tab),
                feature: FeatureName::Engine,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "acceptance-fixture".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    let options = TerminalClientOptions::default();
    let mut wire = TerminalStream::connect_local(&d.socket, terminal.id(), &options)
        .await
        .unwrap();
    assert!(matches!(
        next(&mut wire).await,
        TerminalFrame::Snapshot { .. }
    ));
    let bytes = vec![b'x'; 4096];
    let mut frames = Vec::new();
    let mut total = Duration::ZERO;
    for i in 0..325 {
        let start = Instant::now();
        terminal.push_output(&bytes);
        let TerminalFrame::Output { bytes: received } = next(&mut wire).await else {
            panic!("terminal output")
        };
        assert_eq!(received, bytes);
        if i >= 25 {
            let elapsed = start.elapsed();
            total += elapsed;
            frames.push(elapsed);
        }
    }
    wire.close().await.unwrap();
    let mut reconnect = Vec::new();
    for i in 0..125 {
        let start = Instant::now();
        let mut wire = TerminalStream::connect_local(&d.socket, terminal.id(), &options)
            .await
            .unwrap();
        assert!(matches!(
            next(&mut wire).await,
            TerminalFrame::Snapshot { .. }
        ));
        if i >= 25 {
            reconnect.push(start.elapsed());
        }
        wire.close().await.unwrap();
    }
    let result = serde_json::json!({
        "transport": "owner_unix_socket", "source": "synthetic_terminal_hub",
        "frame_samples": frames.len(), "frame_bytes": bytes.len(),
        "frame_p95_ms": p95(&mut frames),
        "throughput_mib_s": (300.0 * bytes.len() as f64 / 1048576.0) / total.as_secs_f64(),
        "reconnect_samples": reconnect.len(), "reconnect_snapshot_p95_ms": p95(&mut reconnect),
    });
    eprintln!("terminal acceptance baseline: {result}");
    crate::support::save_artifact("terminal-baseline", result);
}
