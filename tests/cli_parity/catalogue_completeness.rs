//! Catalogue completeness parity tests.
//!
//! Confirms that every command documented in `aspec/uxui/cli.md` is present in
//! `CommandCatalogue` and that flag implications / conflicts are registered.

use awman::command::dispatch::catalogue::{CommandCatalogue, FlagKind};

fn cat() -> &'static CommandCatalogue {
    CommandCatalogue::get()
}

// ─── All documented top-level commands are present ───────────────────────────

#[test]
fn all_documented_top_level_commands_present() {
    let names: Vec<&str> = cat().root().subcommands.iter().map(|s| s.name).collect();
    for expected in &[
        "init", "ready", "chat", "specs", "agents", "config", "exec", "api", "remote", "new",
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
    let flag = cmd.find_flag("headless");
    assert!(flag.is_some());
    // Short flag is `-n`.
    assert_eq!(flag.unwrap().short, Some('n'));
}

// ─── api start ──────────────────────────────────────────────────────────

// ─── remote exec ─────────────────────────────────────────────────────────────

#[test]
fn remote_exec_workflow_has_follow_flag() {
    let cmd = cat().lookup(&["remote", "exec", "workflow"]).unwrap();
    assert!(cmd.find_flag("follow").is_some());
}

#[test]
fn remote_exec_workflow_has_workflow_argument() {
    let cmd = cat().lookup(&["remote", "exec", "workflow"]).unwrap();
    assert!(
        cmd.arguments.iter().any(|a| a.name == "workflow"),
        "remote exec workflow must accept a workflow argument"
    );
}

#[test]
fn remote_exec_prompt_has_prompt_argument() {
    let cmd = cat().lookup(&["remote", "exec", "prompt"]).unwrap();
    assert!(
        cmd.arguments.iter().any(|a| a.name == "prompt"),
        "remote exec prompt must accept a prompt argument"
    );
}

// ─── new workflow format values ───────────────────────────────────────────────

#[test]
fn new_workflow_format_accepts_toml_yaml() {
    let cmd = cat().lookup(&["new", "workflow"]).unwrap();
    let flag = cmd.find_flag("format").expect("--format flag");
    if let FlagKind::Enum(values) = flag.kind {
        assert!(values.contains(&"toml"));
        assert!(values.contains(&"yaml"));
        assert!(!values.contains(&"md"), "md format no longer supported");
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

#[test]
fn api_start_settings_are_config_not_flags() {
    // WI 0139: `daemon.network.listen` and `daemon.features.worker.push.workDirs.*`.
    let cmd = cat().lookup(&["api", "start"]).unwrap();
    for removed in ["port", "workdirs", "workdir-root"] {
        assert!(cmd.find_flag(removed).is_none(), "--{removed} must be gone");
    }
}

// ─── session worktree selectors (WI 0153) ────────────────────────────────────

#[test]
fn chat_and_exec_prompt_expose_branch_pr_and_worktree_as_optional_strings() {
    for path in [&["chat"][..], &["exec", "prompt"][..]] {
        let command = cat().lookup(path).unwrap();
        for name in ["branch", "pr", "worktree"] {
            let flag = command
                .find_flag(name)
                .unwrap_or_else(|| panic!("{path:?} lacks --{name}"));
            assert!(
                matches!(flag.kind, FlagKind::OptionalString),
                "{path:?} --{name}"
            );
            for other in ["branch", "pr", "worktree"] {
                if other != name {
                    assert!(flag.conflicts_with(other), "{path:?} --{name} vs --{other}");
                }
            }
        }
    }
}

#[test]
fn exec_workflow_and_remote_exec_prompt_do_not_gain_session_worktree_flags() {
    let workflow = cat().lookup(&["exec", "workflow"]).unwrap();
    assert!(matches!(
        workflow.find_flag("worktree").unwrap().kind,
        FlagKind::Bool
    ));
    assert!(workflow.find_flag("branch").is_none());
    assert!(workflow.find_flag("pr").is_none());
    let remote = cat().lookup(&["remote", "exec", "prompt"]).unwrap();
    for name in ["branch", "pr", "worktree"] {
        assert!(
            remote.find_flag(name).is_none(),
            "remote exec prompt --{name}"
        );
    }
}

/// Every flag the catalogue declares for `chat` and `exec prompt` appears in
/// the committed, generated command reference — the doc cannot lag the
/// catalogue for the new selectors.
#[test]
fn generated_command_reference_lists_the_selectors_for_both_commands() {
    let reference = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/14-command-reference.md"),
    )
    .expect("read docs/14-command-reference.md");
    for flag in ["`--branch`", "`--pr`", "`--worktree`"] {
        assert!(
            reference.matches(flag).count() >= 2,
            "{flag} must be documented for chat and exec prompt"
        );
    }
}
