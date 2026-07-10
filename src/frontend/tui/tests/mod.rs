use std::sync::Arc;
use tokio::sync::RwLock;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use crate::data::session_manager::SessionManager;
use crate::frontend::tui::app::{App, Focus};
use crate::frontend::tui::dialogs::{
    Dialog, DialogResponse, MountScopeState, WorkflowStepErrorState,
};
use crate::frontend::tui::tabs::Tab;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

mod config_show_tests;
mod dialog_tests;
mod key_handler_tests;
mod mouse_handler_tests;
mod render_tests;

// ─── Shared helpers ───────────────────────────────────────────────────────

fn make_engines() -> crate::command::dispatch::Engines {
    let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
    let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from("/tmp")),
    ));
    let git_engine = Arc::new(crate::engine::git::GitEngine::new());
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        runtime.clone(),
    ));
    let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
        crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
        crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
    ));
    let workflow_state_store = {
        let tmp = tempfile::tempdir().unwrap();
        Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
            tmp.path(),
        ))
    };
    crate::command::dispatch::Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine,
        overlay_engine: overlay,
        auth_engine,
        agent_engine,
        workflow_state_store,
    }
}

fn make_session() -> Session {
    let tmp = tempfile::tempdir().unwrap();
    let resolver = StaticGitRootResolver::new(tmp.path());
    Session::open(
        tmp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions::default(),
    )
    .unwrap()
}

fn make_app() -> App {
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let catalogue = CommandCatalogue::get();
    let engines = make_engines();
    let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
    let session = make_session();
    let tab = Tab::new(session);
    App::new(
        catalogue,
        engines,
        session_manager,
        tab,
        rt.handle().clone(),
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
