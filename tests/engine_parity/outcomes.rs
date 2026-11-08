//! Shared Dispatch, dialogs and persisted filesystem effects, compared exactly.
use crate::{
    command::{
        dispatch::{catalogue::CommandCatalogue, parsed_input},
        instance::{
            backend::InstanceBackend, frontend::PlainPresentation, local::LocalInstanceBackend,
            InitialTabOpened, InitialTabSpec, InstanceParts,
        },
    },
    data::{
        config::env::EnvSnapshot,
        dialog::DialogResponse,
        session::{Session, SessionOpenOptions, StaticGitRootResolver},
        session_manager::SessionManager,
    },
    frontend::tui::{
        app::App,
        tabs::{ExecutionPhase, Tab},
    },
    support::{self, Daemon},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
fn frame(app: &mut App) -> ratatui::buffer::Buffer {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| crate::frontend::tui::render::render_frame(app, f))
        .unwrap();
    terminal.backend().buffer().clone()
}
fn tree(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn walk(
        root: &std::path::Path,
        at: &std::path::Path,
        out: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
    ) {
        for e in std::fs::read_dir(at).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                walk(root, &e.path(), out);
            } else {
                out.push((
                    e.path().strip_prefix(root).unwrap().into(),
                    std::fs::read(e.path()).unwrap(),
                ));
            }
        }
    }
    let mut result = vec![];
    walk(root, root, &mut result);
    result.sort();
    result
}
fn session(d: &Daemon) -> Session {
    let hello = d.hello(crate::data::frontend_protocol::FrontendRole::Primary);
    let env = EnvSnapshot::from_instance_env(
        hello.env.into_inner().into_iter().collect(),
        hello.cwd.clone(),
        hello.argv,
    );
    Session::open(
        hello.cwd.clone(),
        &StaticGitRootResolver::new(hello.cwd),
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap()
}
async fn run(backend: Arc<dyn InstanceBackend>, session: Session) -> Vec<ratatui::buffer::Buffer> {
    let InitialTabOpened::Normal { tab } = backend.open_initial(InitialTabSpec::Session {
        session,
        fatal_startup_error: true,
        pty_size: (100, 30),
    }) else {
        panic!("normal tab")
    };
    let (tab, _) = Tab::from_opened(tab);
    let mut app = App::with_backend(backend, tab, tokio::runtime::Handle::current());
    // Includes real local UI requests: the daemon has no presenter, so each
    // request must leave the native frame exactly as builtin does.
    let mut frames = vec![frame(&mut app)];
    app.backend
        .ui_request(crate::data::frontend_protocol::UiRequest::NextTab);
    app.spawn_command(parsed_input::parse("new spec", CommandCatalogue::get()).unwrap());
    let mut answers = vec![
        DialogResponse::Index(0),
        DialogResponse::Text("Conformance gate".into()),
        DialogResponse::Text("first line\nsecond line".into()),
    ]
    .into_iter();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.tick_all_tabs();
        app.poll_dialog_requests();
        if app.active_dialog.is_some() {
            // Dialog frames and session snapshots travel on separate feeds.
            // Wait for the command phase before comparing local and daemon
            // rendering at this dialog boundary.
            while !matches!(
                app.active_tab().execution_phase,
                ExecutionPhase::Running { .. }
            ) {
                assert!(
                    Instant::now() < deadline,
                    "session phase did not reach the dialog"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
                app.tick_all_tabs();
            }
            frames.push(frame(&mut app));
            app.active_tab()
                .dialog_response_tx
                .as_ref()
                .unwrap()
                .send(answers.next().expect("exactly three dialogs"))
                .unwrap();
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        if app.active_tab().command_result_rx.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "command did not finish: backend={:?} remaining answers={} phase={:?} log={:?}",
            app.backend.kind(),
            answers.len(),
            app.active_tab().execution_phase,
            app.active_tab().shared.status_log.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(answers.next().is_none());
    // Synchronize on the engine's terminal session phase before the final frame.
    app.tick_all_tabs();
    frames.push(frame(&mut app));
    frames
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_dialog_content_order_outcomes_files_and_frames_match() {
    let d = Daemon::start(crate::command::integrations::IntegrationRegistry::compiled_in()).await;
    let session = session(&d);
    let initial_files = tree(&d.root.join("repo"));
    let env = session.env().clone();
    let local = LocalInstanceBackend::open(InstanceParts {
        catalogue: CommandCatalogue::get(),
        engines: support::engines(&d.root),
        session_manager: Arc::new(SessionManager::in_memory(env.clone())),
        runtime: tokio::runtime::Handle::current(),
        env: Arc::new(move || env.clone()),
        presentation: Arc::new(PlainPresentation),
    });
    let local_frames = run(local.clone(), session.clone()).await;
    let expected = local.instance().outcome_trace().snapshot();
    assert_eq!(expected.first().unwrap()["kind"], "session_created");
    assert_eq!(
        expected
            .iter()
            .filter(|event| event["kind"] == "dialog")
            .count(),
        3
    );
    assert!(expected.iter().any(|event| event["kind"] == "status"));
    assert_eq!(expected.last().unwrap()["kind"], "result");
    let expected_files = tree(&d.root.join("repo"));
    assert!(!expected_files.is_empty());
    std::fs::remove_dir_all(d.root.join("repo")).unwrap();
    std::fs::create_dir(d.root.join("repo")).unwrap();
    for (path, bytes) in initial_files {
        let path = d.root.join("repo").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let remote = d.remote().await;
    let remote_frames = run(remote.clone(), session).await;
    let actual = d.opened.lock().unwrap()[0].outcome_trace().snapshot();
    support::save_artifact(
        "outcome-traces",
        serde_json::json!({"builtin":expected,"daemon":actual}),
    );
    support::save_artifact(
        "frame-snapshots",
        serde_json::json!({"builtin":local_frames.iter().map(|f|format!("{f:?}")).collect::<Vec<_>>(),"daemon":remote_frames.iter().map(|f|format!("{f:?}")).collect::<Vec<_>>() }),
    );
    assert_eq!(actual, expected, "engine-side outcome traces");
    assert_eq!(
        tree(&d.root.join("repo")),
        expected_files,
        "git/worktree/scaffold effects"
    );
    assert_eq!(remote_frames.len(), local_frames.len(), "snapshot count");
    for (index, (actual, expected)) in remote_frames.iter().zip(&local_frames).enumerate() {
        assert_eq!(actual.area, expected.area);
        let differences = actual
            .content
            .iter()
            .zip(&expected.content)
            .enumerate()
            .filter(|(_, (a, e))| a != e)
            .collect::<Vec<_>>();
        assert!(
            differences.is_empty(),
            "frame {index}: {} cell differences; first cells: {:?}",
            differences.len(),
            differences.iter().take(24).collect::<Vec<_>>()
        );
    }
    remote.quit();
    local.quit();
}
