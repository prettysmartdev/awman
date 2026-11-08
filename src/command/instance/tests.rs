//! Unit tests for `EngineInstance` and `LocalInstanceBackend` (WI 0142 §5
//! refactor): tab registry, session ownership, submission plumbing, the
//! instance-owned startup command, command replacement, close/quit/drop
//! lifecycle, PTY size state and the unclaimed `UiRequest` path.

use std::sync::Arc;

use super::backend::InstanceBackend;
use super::feeds::{SharedPtySize, TabCommandFeeds};
use super::frontend::PlainPresentation;
use super::local::LocalInstanceBackend;
use super::*;
use crate::command::dispatch::Engines;
use crate::data::config::env::EnvSnapshot;
use crate::data::session_manager::SessionManager;

fn runtime() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| tokio::runtime::Runtime::new().unwrap())
        .handle()
        .clone()
}

/// A runtime nobody drives: a command spawned on it never starts, so job
/// tracking and cancellation can be observed without a command finishing
/// (or touching the host) in between.
fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn session_root() -> &'static std::path::Path {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

fn backend_on(engines: Engines, runtime: tokio::runtime::Handle) -> Arc<LocalInstanceBackend> {
    LocalInstanceBackend::open(InstanceParts {
        catalogue: CommandCatalogue::get(),
        engines,
        session_manager: Arc::new(SessionManager::in_memory(EnvSnapshot::empty())),
        runtime,
        env: Arc::new(EnvSnapshot::empty),
        presentation: Arc::new(PlainPresentation),
    })
}

fn test_engines() -> Engines {
    Engines::for_tests(std::path::Path::new("/tmp"))
}

fn backend_with(engines: Engines) -> Arc<LocalInstanceBackend> {
    backend_on(engines, runtime())
}

fn backend() -> Arc<LocalInstanceBackend> {
    backend_with(test_engines())
}

/// A backend on a runtime that never runs its commands.
fn paused_backend(runtime: &tokio::runtime::Runtime) -> Arc<LocalInstanceBackend> {
    backend_on(test_engines(), runtime.handle().clone())
}

fn submit_startup(
    backend: &LocalInstanceBackend,
    session: &Session,
    feeds: TabCommandFeeds,
) -> Result<SubmittedCommand, CommandError> {
    backend.submit(SubmitRequest {
        session_id: session.id(),
        parsed: backend.instance().startup_command(session),
        feeds,
        pty_size: (80, 24),
        squad_attach: None,
    })
}

#[test]
fn initial_session_tab_registers_the_session_and_starts_its_startup_command() {
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    let session = Session::for_tests(session_root());
    let id = session.id();
    let InitialTabOpened::Normal { tab } = backend.open_initial(InitialTabSpec::Session {
        session: session.clone(),
        fatal_startup_error: false,
        pty_size: (120, 40),
    }) else {
        panic!("a session spec opens a normal tab");
    };
    assert_eq!(tab.session.id(), id);
    assert!(backend.session_snapshot(&id).is_some());
    assert!(
        matches!(tab.startup, StartupRun::Started(_)),
        "the instance starts the startup command itself"
    );
    assert_eq!(backend.instance().tab_ids().len(), 1);
    assert_eq!(backend.running_jobs(), 1);
    assert_eq!(backend.instance().tab_pty_size(&id), Some((120, 40)));
    // The run is bound to the feeds the presenter's tab will present.
    backend.quit();
    assert!(tab.feeds.command_cancel.is_cancelled());
}

#[test]
fn a_fatal_start_opens_the_tab_without_a_startup_command() {
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    let InitialTabOpened::Normal { tab } = backend.open_initial(InitialTabSpec::Session {
        session: Session::for_tests(session_root()),
        fatal_startup_error: true,
        pty_size: (80, 24),
    }) else {
        panic!("a session spec opens a normal tab");
    };
    assert!(matches!(tab.startup, StartupRun::NotStarted));
    assert_eq!(backend.running_jobs(), 0);
}

#[test]
fn opening_a_tab_starts_its_startup_command_in_the_instance() {
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    backend.adopt_session(&Session::for_tests(session_root()));
    let opened = backend
        .open_tab(OpenTabRequest {
            dir: session_root().to_path_buf(),
            options: SessionOpenOptions::default(),
            pty_size: (100, 30),
        })
        .expect("a directory opens");
    let id = opened.session.id();
    assert!(matches!(opened.startup, StartupRun::Started(_)));
    assert!(backend.tab_id(&id).is_some());
    assert_eq!(backend.running_jobs(), 1);
    assert_eq!(backend.instance().tab_pty_size(&id), Some((100, 30)));
    let recorded = backend
        .session_snapshot(&id)
        .and_then(|s| s.state().current_command.clone())
        .map(|c| c.subcommand);
    assert_eq!(
        recorded,
        Some(
            backend
                .instance()
                .startup_command(&opened.session)
                .path
                .join(" ")
        ),
        "the start is recorded on the session before open_tab returns"
    );
}

#[test]
fn squad_without_a_container_runtime_degrades_to_the_fallback_tab() {
    let mut engines = test_engines();
    engines.container_runtime = None;
    let backend = backend_with(engines);
    let fallback = Session::for_tests(session_root());
    let id = fallback.id();
    let InitialTabOpened::SquadFailed { tab, error } =
        backend.open_initial(InitialTabSpec::Squad { fallback })
    else {
        panic!("squad cannot open without a container runtime");
    };
    assert_eq!(tab.session.id(), id);
    assert!(error
        .to_string()
        .contains("squad requires a container runtime"));
    assert!(
        backend.session_snapshot(&id).is_some(),
        "fallback is registered"
    );
    assert!(matches!(tab.startup, StartupRun::NotStarted));
}

#[test]
fn opening_the_squad_session_registers_its_tab() {
    let backend = backend();
    let normal = Session::for_tests(session_root());
    backend.adopt_session(&normal);
    let squad = backend.open_squad_session().expect("the squad root opens");
    assert!(
        backend.tab_id(&squad.id()).is_some(),
        "the squad tab is in the instance's order as soon as it opens"
    );
    assert_eq!(backend.instance().tab_ids().len(), 2);
    // Closing the normal tab leaves the squad tab: not the last tab.
    assert_eq!(backend.close_tab(&normal.id()), CloseTabOutcome::Closed);
    assert!(!backend.instance().has_ended());
}

#[test]
fn adopt_is_idempotent_and_close_keeps_order() {
    let backend = backend();
    let a = Session::for_tests(session_root());
    let b = Session::for_tests(session_root());
    backend.adopt_session(&a);
    backend.adopt_session(&a);
    backend.adopt_session(&b);
    assert_eq!(backend.instance().tab_ids().len(), 2);
    let tab_b = backend.tab_id(&b.id()).unwrap();
    assert_eq!(backend.close_tab(&a.id()), CloseTabOutcome::Closed);
    assert_eq!(backend.instance().tab_ids(), vec![tab_b]);
    assert_eq!(backend.close_tab(&b.id()), CloseTabOutcome::ClosedLastTab);
    assert!(backend.instance().tab_ids().is_empty());
    assert!(
        backend.instance().has_ended(),
        "closing the last tab ends the instance"
    );
}

#[test]
fn ui_requests_are_never_claimed_by_the_builtin_backend() {
    let backend = backend();
    let ticket = backend.ui_request(crate::data::frontend_protocol::UiRequest::NextTab);
    let events = backend.poll();
    assert!(matches!(
        events.as_slice(),
        [backend::BackendEvent::UiAck { ticket: t, outcome }]
            if *t == ticket && !outcome.claimed_by_host
    ));
    assert!(backend.poll().is_empty());
    assert!(backend.health().is_ok());
    assert_eq!(backend.kind(), backend::BackendKind::Builtin);
}

#[test]
fn submit_without_a_registered_session_is_refused_and_spawns_nothing() {
    let backend = backend();
    let session = Session::for_tests(session_root());
    let result = submit_startup(&backend, &session, TabCommandFeeds::new());
    assert!(result.is_err());
    assert_eq!(backend.running_jobs(), 0);
    assert!(
        backend.tab_id(&session.id()).is_none(),
        "a refused submission registers no tab"
    );
}

#[test]
fn submit_records_the_start_on_the_session_before_returning() {
    // The command never runs on a paused runtime, so the only writer of
    // `current_command` is `submit` itself.
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    let session = Session::for_tests(session_root());
    let id = session.id();
    backend.adopt_session(&session);
    let name = backend.instance().startup_command(&session).path.join(" ");
    let _submitted =
        submit_startup(&backend, &session, TabCommandFeeds::new()).expect("submission spawns");
    let recorded = backend
        .session_snapshot(&id)
        .and_then(|s| s.state().current_command.clone())
        .map(|c| c.subcommand);
    assert_eq!(recorded, Some(name));
}

#[test]
fn a_second_submission_cancels_and_keeps_the_first_run() {
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    let session = Session::for_tests(session_root());
    backend.adopt_session(&session);
    let first = TabCommandFeeds::new();
    let first_cancel = first.command_cancel.clone();
    let _first = submit_startup(&backend, &session, first).unwrap();
    let second = TabCommandFeeds::new();
    let second_cancel = second.command_cancel.clone();
    let _second = submit_startup(&backend, &session, second).unwrap();

    assert!(
        first_cancel.is_cancelled(),
        "the replaced command is cancelled by the instance"
    );
    assert!(!second_cancel.is_cancelled());
    assert_eq!(
        backend.running_jobs(),
        2,
        "the replaced run counts until its thread finishes"
    );
    backend.quit();
    assert!(second_cancel.is_cancelled());
}

#[test]
fn a_closed_tab_admits_no_stale_submission() {
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    let a = Session::for_tests(session_root());
    let b = Session::for_tests(session_root());
    backend.adopt_session(&a);
    backend.adopt_session(&b);
    let feeds = TabCommandFeeds::new();
    let running = feeds.command_cancel.clone();
    let _run = submit_startup(&backend, &a, feeds).unwrap();

    assert_eq!(backend.close_tab(&a.id()), CloseTabOutcome::Closed);
    assert!(running.is_cancelled(), "closing cancels the tab's command");
    assert_eq!(backend.running_jobs(), 1, "kept until it finishes");

    let stale = TabCommandFeeds::new();
    let stale_cancel = stale.command_cancel.clone();
    assert!(submit_startup(&backend, &a, stale).is_err());
    assert!(
        backend.tab_id(&a.id()).is_none(),
        "a stale submission must not resurrect the closed tab"
    );
    assert_eq!(backend.running_jobs(), 1, "nothing new was spawned");
    assert!(!stale_cancel.is_cancelled());
}

#[test]
fn close_and_submit_race_never_leaves_an_unowned_run() {
    // Concurrent callers through the `Send + Sync` facade: whichever wins,
    // every spawned run is owned (counted) and, after the close, cancelled.
    let runtime = paused_runtime();
    for _ in 0..50 {
        let backend = paused_backend(&runtime);
        let a = Session::for_tests(session_root());
        let keep = Session::for_tests(session_root());
        backend.adopt_session(&a);
        backend.adopt_session(&keep);
        let feeds = TabCommandFeeds::new();
        let cancel = feeds.command_cancel.clone();
        let submitter = {
            let backend = Arc::clone(&backend);
            let a = a.clone();
            std::thread::spawn(move || submit_startup(&backend, &a, feeds).is_ok())
        };
        backend.close_tab(&a.id());
        let spawned = submitter.join().unwrap();
        assert_eq!(backend.running_jobs(), usize::from(spawned));
        if spawned {
            assert!(
                cancel.is_cancelled(),
                "a run admitted before the close is cancelled by it"
            );
        }
        assert!(backend.tab_id(&a.id()).is_none());
    }
}

#[test]
fn quit_ends_the_instance_and_cancels_every_running_command() {
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    let session = Session::for_tests(session_root());
    backend.adopt_session(&session);
    let feeds = TabCommandFeeds::new();
    let cancel = feeds.command_cancel.clone();
    let _submitted = submit_startup(&backend, &session, feeds).unwrap();
    let plan = backend.quit();
    assert!(cancel.is_cancelled());
    assert_eq!(
        plan,
        QuitPlan {
            detach_containers: false,
            exit_code: 0,
            cancel_running: true
        }
    );
    assert!(backend.instance().has_ended());
    assert!(
        submit_startup(&backend, &session, TabCommandFeeds::new()).is_err(),
        "an ended instance admits no new command"
    );
    assert!(backend
        .open_tab(OpenTabRequest {
            dir: session_root().to_path_buf(),
            options: SessionOpenOptions::default(),
            pty_size: (80, 24),
        })
        .is_err());
    assert_eq!(backend.quit(), plan, "quit is idempotent");
}

#[test]
fn dropping_the_instance_cancels_its_commands() {
    let runtime = paused_runtime();
    let backend = paused_backend(&runtime);
    let session = Session::for_tests(session_root());
    backend.adopt_session(&session);
    let feeds = TabCommandFeeds::new();
    let cancel = feeds.command_cancel.clone();
    let _submitted = submit_startup(&backend, &session, feeds).unwrap();
    drop(backend);
    assert!(
        cancel.is_cancelled(),
        "no command outlives its instance uncancelled"
    );
}

#[test]
fn resize_updates_the_size_later_launches_in_the_tab_use() {
    let backend = backend();
    let session = Session::for_tests(session_root());
    backend.adopt_session(&session);
    assert_eq!(
        backend.instance().tab_pty_size(&session.id()),
        Some(SharedPtySize::DEFAULT)
    );
    backend.resize_tab(&session.id(), (137, 41));
    assert_eq!(
        backend.instance().tab_pty_size(&session.id()),
        Some((137, 41))
    );
}

#[test]
fn workflow_launches_use_the_presenter_supplied_pty_size() {
    use crate::command::commands::agent_setup::HasAgentFrontend;
    use crate::data::workflow_definition::WorkflowStep;
    use crate::engine::workflow::frontend::WorkflowFrontend;

    let (frontend, _req_rx, _resp_tx) =
        super::test_support::test_command_frontend(&["exec", "workflow"], Default::default());
    let size = SharedPtySize::new((137, 41));
    let mut frontend = frontend.with_pty_size(size.clone());
    let step = WorkflowStep {
        name: "first".into(),
        depends_on: vec![],
        prompt_template: "test".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };
    frontend.report_step_interactive_launch(&step, "agent", None);
    let mut proxy = frontend.container_frontend_for_pty();
    assert_eq!(proxy.take_io().initial_size, Some((137, 41)));

    // A resize reported between launches applies to the next one.
    size.set((100, 30));
    frontend.report_step_interactive_launch(&step, "agent", None);
    let mut proxy = frontend.container_frontend_for_pty();
    assert_eq!(proxy.take_io().initial_size, Some((100, 30)));
}

#[test]
fn a_frontend_keeps_its_initial_size_for_later_launches() {
    use crate::command::commands::agent_setup::HasAgentFrontend;
    use crate::data::workflow_definition::WorkflowStep;
    use crate::engine::workflow::frontend::WorkflowFrontend;

    let (req_tx, _req_rx) = std::sync::mpsc::channel();
    let (_resp_tx, resp_rx) = std::sync::mpsc::channel();
    let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
    let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut frontend = InstanceCommandFrontend::new(
        ParsedCommandBoxInput {
            path: vec!["exec".into(), "workflow".into()],
            args: Default::default(),
        },
        DialogChannels::new(req_tx, resp_rx),
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout: out_tx.clone(),
            stderr: out_tx,
            stdin_tx: in_tx,
            stdin_rx: in_rx,
            resize: None,
            initial_size: Some((137, 41)),
        },
        TabCommandFeeds::new(),
    );
    let step = WorkflowStep {
        name: "first".into(),
        depends_on: vec![],
        prompt_template: "test".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };
    frontend.report_step_interactive_launch(&step, "agent", None);
    let mut proxy = frontend.container_frontend_for_pty();
    assert_eq!(proxy.take_io().initial_size, Some((137, 41)));
}
