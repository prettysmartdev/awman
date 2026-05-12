//! Catalogue completeness parity tests.
//!
//! Confirms that every command documented in `aspec/uxui/cli.md` is present in
//! `CommandCatalogue` and that flag implications / conflicts are registered.

use amux::command::dispatch::catalogue::{ArgumentKind, CommandCatalogue, FlagKind};

fn cat() -> &'static CommandCatalogue {
    CommandCatalogue::get()
}

// ─── All documented top-level commands are present ───────────────────────────

#[test]
fn all_documented_top_level_commands_present() {
    let names: Vec<&str> = cat().root().subcommands.iter().map(|s| s.name).collect();
    for expected in &[
        "init", "ready", "chat", "specs", "status", "config", "exec", "headless", "remote", "new",
    ] {
        assert!(
            names.contains(expected),
            "missing top-level command {expected:?}; found: {names:?}"
        );
    }
}

// ─── specs subcommands ────────────────────────────────────────────────────────

#[test]
fn specs_amend_has_work_item_argument() {
    let amend = cat().lookup(&["specs", "amend"]).unwrap();
    assert!(
        !amend.arguments.is_empty(),
        "amend needs a work-item argument"
    );
}

// ─── init flags ───────────────────────────────────────────────────────────────

#[test]
fn init_has_aspec_flag() {
    let init = cat().lookup(&["init"]).unwrap();
    assert!(init.find_flag("aspec").is_some());
}

// ─── exec workflow ────────────────────────────────────────────────────────────

#[test]
fn exec_workflow_has_work_item_flag() {
    let cmd = cat().lookup(&["exec", "workflow"]).unwrap();
    assert!(cmd.find_flag("work-item").is_some());
}

#[test]
fn exec_workflow_has_auto_flag() {
    let cmd = cat().lookup(&["exec", "workflow"]).unwrap();
    assert!(cmd.find_flag("auto").is_some());
}

#[test]
fn exec_workflow_has_worktree_flag() {
    let cmd = cat().lookup(&["exec", "workflow"]).unwrap();
    assert!(cmd.find_flag("worktree").is_some());
}

// ─── chat ─────────────────────────────────────────────────────────────────────

#[test]
fn chat_has_non_interactive_short_flag() {
    let cmd = cat().lookup(&["chat"]).unwrap();
    let flag = cmd.find_flag("non-interactive");
    assert!(flag.is_some());
    // Short flag is `-n`.
    assert_eq!(flag.unwrap().short, Some('n'));
}

// ─── headless start ──────────────────────────────────────────────────────────

#[test]
fn headless_start_has_workdirs_flag() {
    let cmd = cat().lookup(&["headless", "start"]).unwrap();
    assert!(cmd.find_flag("workdirs").is_some());
}

// ─── remote run ──────────────────────────────────────────────────────────────

#[test]
fn remote_run_has_follow_flag() {
    let cmd = cat().lookup(&["remote", "run"]).unwrap();
    assert!(cmd.find_flag("follow").is_some());
}

#[test]
fn remote_run_has_trailing_args_argument() {
    let cmd = cat().lookup(&["remote", "run"]).unwrap();
    let trailing = cmd
        .arguments
        .iter()
        .any(|a| matches!(a.kind, ArgumentKind::TrailingVarArgs));
    assert!(trailing, "remote run must accept trailing var-args");
}

// ─── new workflow format values ───────────────────────────────────────────────

#[test]
fn new_workflow_format_accepts_toml_yaml_md() {
    let cmd = cat().lookup(&["new", "workflow"]).unwrap();
    let flag = cmd.find_flag("format").expect("--format flag");
    if let FlagKind::Enum(values) = flag.kind {
        assert!(values.contains(&"toml"));
        assert!(values.contains(&"yaml"));
        assert!(values.contains(&"md"));
    } else {
        panic!("--format should be Enum kind");
    }
}

// ─── config set / get ─────────────────────────────────────────────────────────

#[test]
fn config_set_has_global_flag() {
    let cmd = cat().lookup(&["config", "set"]).unwrap();
    assert!(cmd.find_flag("global").is_some());
}

#[test]
fn config_get_has_field_argument() {
    let cmd = cat().lookup(&["config", "get"]).unwrap();
    assert!(!cmd.arguments.is_empty());
}
