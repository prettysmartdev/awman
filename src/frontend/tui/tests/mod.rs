use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::Engines;
use crate::data::session::{Session, SessionOpenOptions};
use crate::data::session_manager::SessionManager;
use crate::frontend::tui::app::{App, Focus};
use crate::frontend::tui::dialogs::{Dialog, DialogResponse, MountScopeState};
use crate::frontend::tui::tabs::Tab;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use std::sync::Arc;

mod config_show_tests;
mod dialog_tests;
mod key_handler_tests;
mod mouse_handler_tests;
mod render_tests;

// ─── Shared helpers ───────────────────────────────────────────────────────

// ─── The one TUI test fixture for a command frontend ──────────────────────

/// Build a [`TuiCommandFrontend`] for a unit test, plus the two dialog
/// endpoints the test drives it from.
///
/// This is the **only** place a TUI unit test hand-builds a
/// [`ParsedCommandBoxInput`]. Five per-command test modules each had their own
/// fifty-line copy of this; collapsing them means the "a frontend must not
/// invent a command path" guard has one allowlisted call site instead of six.
pub(crate) fn test_command_frontend(
    path: &[&str],
    flags: std::collections::BTreeMap<String, crate::command::dispatch::parsed_input::FlagValue>,
) -> (
    crate::frontend::tui::command_frontend::TuiCommandFrontend,
    std::sync::mpsc::Receiver<crate::frontend::tui::dialogs::DialogRequest>,
    std::sync::mpsc::Sender<DialogResponse>,
) {
    use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
    use crate::engine::agent_runtime::frontend::AgentIo;
    use crate::frontend::tui::command_frontend::{DialogChannels, TuiCommandFrontend};
    use crate::frontend::tui::dialogs::DialogRequest;
    use crate::frontend::tui::tabs::TabSharedState;

    let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
    let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
    let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    let (stderr_tx, _stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let container_io = AgentIo {
        stdout: stdout_tx,
        stderr: stderr_tx,
        stdin_tx,
        stdin_rx,
        resize: Some(resize_rx),
        initial_size: Some((80, 24)),
    };
    let frontend = TuiCommandFrontend::new(
        ParsedCommandBoxInput {
            path: path.iter().map(|s| s.to_string()).collect(),
            flags,
            arguments: Default::default(),
        },
        DialogChannels::new(req_tx, resp_rx),
        container_io,
        TabSharedState::for_tests(),
    );
    (frontend, req_rx, resp_tx)
}

fn make_session() -> Session {
    // Sessions retain their workdir path, so it must outlive each test's App.
    // Keep one root for this test process rather than leaking a TempDir for
    // every fixture.  The latter creates thousands of orphaned directories
    // during `make test` and eventually exhausts the temporary-root directory.
    static TEST_SESSION_ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = TEST_SESSION_ROOT
        .get_or_init(|| tempfile::tempdir().expect("create TUI test session root"));
    Session::for_tests(root.path())
}

/// One multi-threaded runtime shared by every test in this binary that needs
/// a `Handle`, rather than each test leaking its own: leaking a fresh
/// `Runtime` (and its worker-thread pool) per call, across the hundreds of
/// tests in this module tree, exhausts the OS thread/process budget in a
/// resource-constrained CI container well before the suite finishes.
pub(super) fn test_runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| tokio::runtime::Runtime::new().unwrap())
        .handle()
        .clone()
}

fn make_app() -> App {
    let catalogue = CommandCatalogue::get();
    let engines = Engines::for_tests(std::path::Path::new("/tmp"));
    let session_manager = Arc::new(SessionManager::in_memory());
    let session = make_session();
    let tab = Tab::new(session);
    App::new(
        catalogue,
        engines,
        session_manager,
        tab,
        test_runtime_handle(),
    )
}

fn press_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    super::key_handler::handle_key_event(
        app,
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
}

fn activate_workflow_context(app: &mut App, path: &str) {
    app.active_tab_mut().execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
        command: "exec workflow".into(),
    };
    *app.active_tab()
        .shared
        .workflow_context_path
        .lock()
        .unwrap() = Some(path.into());
}

fn press_char(app: &mut App, c: char) {
    press_key(app, KeyCode::Char(c), KeyModifiers::NONE);
}

fn setup_command_dialog(
    app: &mut App,
    dialog: Dialog,
) -> std::sync::mpsc::Receiver<DialogResponse> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[app.active_tab].dialog_response_tx = Some(tx);
    app.active_dialog = Some(dialog);
    app.command_dialog_active = true;
    rx
}

// ─── Clap routing (existing tests retained) ───────────────────────────────

#[test]
fn bare_invocation_has_no_subcommand() {
    let cmd = CommandCatalogue::get().build_clap_command();
    let m = cmd.try_get_matches_from(["awman"]).unwrap();
    assert!(
        m.subcommand_name().is_none(),
        "bare `awman` must have no subcommand — main.rs uses this to route to TUI"
    );
}

#[test]
fn subcommand_presence_routes_away_from_tui() {
    let cmd = CommandCatalogue::get().build_clap_command();
    for argv in [
        vec!["awman", "status"],
        vec!["awman", "ready"],
        vec!["awman", "chat"],
    ] {
        let m = cmd.clone().try_get_matches_from(&argv).unwrap();
        assert!(
            m.subcommand_name().is_some(),
            "{argv:?} must have a subcommand name"
        );
    }
}
