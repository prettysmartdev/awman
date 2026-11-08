//! Verifies that every command documented as having `--json` output does so.
//!
//! This is a catalogue-level check — no subprocess invocation.

use awman::command::dispatch::catalogue::CommandCatalogue;

fn cat() -> &'static CommandCatalogue {
    CommandCatalogue::get()
}

#[test]
fn ready_json_flag_exists_in_catalogue() {
    let cmd = cat().lookup(&["ready"]).unwrap();
    assert!(
        cmd.find_flag("json").is_some(),
        "`ready` must have a --json flag for machine-readable output"
    );
}

#[test]
fn status_watch_flag_exists_in_catalogue() {
    // `agents status` has --watch; this test confirms the flag shape
    // described in the parity matrix item 13.
    let cmd = cat().lookup(&["agents", "status"]).unwrap();
    assert!(
        cmd.find_flag("watch").is_some(),
        "`agents status` must have a --watch flag"
    );
}

#[test]
fn ready_non_interactive_implied_by_json() {
    // Per parity item 3: `--json` implies `--headless`.
    let cmd = cat().lookup(&["ready"]).unwrap();
    let json_flag = cmd.find_flag("json").expect("--json flag");
    assert!(
        json_flag.implies.contains(&"headless"),
        "--json must imply --headless; implies = {:?}",
        json_flag.implies
    );
}

#[test]
fn agents_json_state_spellings_for_reported_status() {
    use awman::command::commands::agents::AgentState;
    for (state, spelling) in [
        (AgentState::Waiting, "waiting"),
        (AgentState::Errored, "errored"),
        (AgentState::Done, "done"),
        (AgentState::Stuck, "stuck"),
        (AgentState::Running, "running"),
    ] {
        assert_eq!(
            serde_json::to_value(state).unwrap(),
            serde_json::json!(spelling),
            "`agents status --json` spells {state:?} as {spelling}"
        );
        assert_eq!(state.label(), spelling);
    }
}
