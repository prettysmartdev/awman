//! Verifies that every command documented as having `--json` output does so.
//!
//! This is a catalogue-level check — no subprocess invocation.

use amux::command::dispatch::catalogue::CommandCatalogue;

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
    // `status` has --watch (not --json); this test confirms the flag shape
    // described in the parity matrix item 13.
    let cmd = cat().lookup(&["status"]).unwrap();
    assert!(
        cmd.find_flag("watch").is_some(),
        "`status` must have a --watch flag"
    );
}

#[test]
fn ready_non_interactive_implied_by_json() {
    // Per parity item 3: `--json` implies `--non-interactive`.
    let cmd = cat().lookup(&["ready"]).unwrap();
    let json_flag = cmd.find_flag("json").expect("--json flag");
    assert!(
        json_flag.implies.contains(&"non-interactive"),
        "--json must imply --non-interactive; implies = {:?}",
        json_flag.implies
    );
}
