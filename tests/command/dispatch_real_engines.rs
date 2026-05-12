//! `CommandCatalogue` completeness and dispatch invariant tests.
//!
//! Parity matrix items 1–22 require the binary as a subprocess.
//! This file verifies the catalogue itself (path lookup, alias resolution,
//! flag enumeration) which is a pure in-memory check.

use amux::command::dispatch::catalogue::CommandCatalogue;

// ─── Top-level command coverage ──────────────────────────────────────────────

fn top_level_names() -> Vec<&'static str> {
    CommandCatalogue::get()
        .root()
        .subcommands
        .iter()
        .map(|s| s.name)
        .collect()
}

#[test]
fn catalogue_has_init_command() {
    assert!(top_level_names().contains(&"init"));
}

#[test]
fn catalogue_has_ready_command() {
    assert!(top_level_names().contains(&"ready"));
}

#[test]
fn catalogue_has_chat_command() {
    assert!(top_level_names().contains(&"chat"));
}

#[test]
fn catalogue_has_specs_command() {
    assert!(top_level_names().contains(&"specs"));
}

#[test]
fn catalogue_has_status_command() {
    assert!(top_level_names().contains(&"status"));
}

#[test]
fn catalogue_has_config_command() {
    assert!(top_level_names().contains(&"config"));
}

#[test]
fn catalogue_has_exec_command() {
    assert!(top_level_names().contains(&"exec"));
}

#[test]
fn catalogue_has_headless_command() {
    assert!(top_level_names().contains(&"headless"));
}

#[test]
fn catalogue_has_remote_command() {
    assert!(top_level_names().contains(&"remote"));
}

#[test]
fn catalogue_has_new_command() {
    assert!(top_level_names().contains(&"new"));
}

// ─── Subcommand coverage ─────────────────────────────────────────────────────

fn subcommand_names(parent: &str) -> Vec<&'static str> {
    CommandCatalogue::get()
        .lookup(&[parent])
        .expect("parent command must exist")
        .subcommands
        .iter()
        .map(|s| s.name)
        .collect()
}

#[test]
fn specs_has_amend_subcommand() {
    let names = subcommand_names("specs");
    assert!(names.contains(&"amend"), "missing 'amend': {names:?}");
}

#[test]
fn config_has_show_get_set_subcommands() {
    let names = subcommand_names("config");
    assert!(names.contains(&"show"));
    assert!(names.contains(&"get"));
    assert!(names.contains(&"set"));
}

#[test]
fn exec_has_prompt_and_workflow_subcommands() {
    let names = subcommand_names("exec");
    assert!(names.contains(&"prompt"));
    assert!(names.contains(&"workflow"));
}

#[test]
fn headless_has_start_kill_logs_status_subcommands() {
    let names = subcommand_names("headless");
    assert!(names.contains(&"start"));
    assert!(names.contains(&"kill"));
    assert!(names.contains(&"logs"));
    assert!(names.contains(&"status"));
}

#[test]
fn remote_has_run_and_session_subcommands() {
    let names = subcommand_names("remote");
    assert!(names.contains(&"run"));
    assert!(names.contains(&"session"));
}

#[test]
fn new_has_spec_workflow_skill_subcommands() {
    let names = subcommand_names("new");
    assert!(names.contains(&"spec"));
    assert!(names.contains(&"workflow"));
    assert!(names.contains(&"skill"));
}

#[test]
fn remote_session_has_start_and_kill_subcommands() {
    let cat = CommandCatalogue::get();
    let remote = cat.lookup(&["remote"]).unwrap();
    let session = remote.find_subcommand("session").unwrap();
    let sub_names: Vec<&str> = session.subcommands.iter().map(|s| s.name).collect();
    assert!(sub_names.contains(&"start"));
    assert!(sub_names.contains(&"kill"));
}

// ─── Flag enumeration ─────────────────────────────────────────────────────────

#[test]
fn init_has_agent_flag() {
    let cat = CommandCatalogue::get();
    let init = cat.lookup(&["init"]).unwrap();
    assert!(
        init.find_flag("agent").is_some(),
        "init must have --agent flag"
    );
}

#[test]
fn init_agent_flag_accepts_known_agents() {
    use amux::command::dispatch::catalogue::FlagKind;
    let cat = CommandCatalogue::get();
    let init = cat.lookup(&["init"]).unwrap();
    let flag = init.find_flag("agent").unwrap();
    if let FlagKind::Enum(values) = flag.kind {
        for agent in &[
            "claude", "codex", "opencode", "maki", "gemini", "copilot", "crush", "cline",
        ] {
            assert!(
                values.contains(agent),
                "agent {agent:?} not in enum values: {values:?}"
            );
        }
    } else {
        panic!("--agent should be Enum kind");
    }
}

#[test]
fn ready_has_build_flag() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    assert!(ready.find_flag("build").is_some());
}

#[test]
fn ready_has_no_cache_flag() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    assert!(ready.find_flag("no-cache").is_some());
}

#[test]
fn ready_has_json_flag() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    assert!(
        ready.find_flag("json").is_some(),
        "ready must have --json flag for machine-readable output"
    );
}

#[test]
fn exec_workflow_has_yolo_flag() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["exec", "workflow"]).unwrap();
    assert!(wf.find_flag("yolo").is_some());
}

#[test]
fn exec_workflow_has_wf_alias() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["exec", "workflow"]).unwrap();
    assert!(
        wf.aliases.contains(&"wf"),
        "`exec workflow` must have 'wf' alias"
    );
}

#[test]
fn headless_start_has_port_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["headless", "start"]).unwrap();
    assert!(start.find_flag("port").is_some());
}

#[test]
fn headless_start_has_background_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["headless", "start"]).unwrap();
    assert!(start.find_flag("background").is_some());
}

#[test]
fn headless_start_has_refresh_key_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["headless", "start"]).unwrap();
    assert!(start.find_flag("refresh-key").is_some());
}

#[test]
fn headless_start_has_dangerously_skip_auth_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["headless", "start"]).unwrap();
    assert!(start.find_flag("dangerously-skip-auth").is_some());
}

#[test]
fn new_workflow_has_format_flag() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["new", "workflow"]).unwrap();
    assert!(wf.find_flag("format").is_some());
}

#[test]
fn new_workflow_has_global_flag() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["new", "workflow"]).unwrap();
    assert!(wf.find_flag("global").is_some());
}

#[test]
fn new_skill_has_global_flag() {
    let cat = CommandCatalogue::get();
    let skill = cat.lookup(&["new", "skill"]).unwrap();
    assert!(skill.find_flag("global").is_some());
}

#[test]
fn status_has_watch_flag() {
    let cat = CommandCatalogue::get();
    let status = cat.lookup(&["status"]).unwrap();
    assert!(status.find_flag("watch").is_some());
}

#[test]
fn lookup_nonexistent_command_returns_none() {
    let cat = CommandCatalogue::get();
    assert!(cat.lookup(&["nonexistent"]).is_none());
}

#[test]
fn lookup_deeply_nested_path() {
    let cat = CommandCatalogue::get();
    assert!(cat.lookup(&["remote", "session", "start"]).is_some());
    assert!(cat.lookup(&["remote", "session", "kill"]).is_some());
}
