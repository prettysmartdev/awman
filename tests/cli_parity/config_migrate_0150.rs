//! `config migrate` catalogue parity (WI 0150).
//!
//! One catalogue entry drives the CLI parser, the TUI command box and the API
//! admission check, so these tests pin what each frontend can reach and that
//! the clap command the CLI parses from the catalogue carries the same flags.

use awman::command::dispatch::catalogue::{CommandCatalogue, FlagKind, FrontendKind};

fn cat() -> &'static CommandCatalogue {
    CommandCatalogue::get()
}

const PATH: &[&str] = &["config", "migrate"];

#[test]
fn config_migrate_is_registered_under_config_beside_set() {
    let config = cat().lookup(&["config"]).expect("config");
    let names: Vec<&str> = config.subcommands.iter().map(|s| s.name).collect();
    assert!(names.contains(&"migrate"), "config subcommands: {names:?}");
    assert!(names.contains(&"set"), "config subcommands: {names:?}");
}

#[test]
fn config_migrate_declares_the_documented_flags() {
    let migrate = cat().lookup(PATH).expect("config migrate");
    for flag in ["yes", "dry-run", "global", "non-interactive"] {
        let spec = migrate
            .find_flag(flag)
            .unwrap_or_else(|| panic!("config migrate lacks --{flag}"));
        assert!(
            matches!(spec.kind, FlagKind::Bool),
            "--{flag} must be a boolean flag"
        );
    }
    assert_eq!(migrate.find_flag("yes").unwrap().short, Some('y'));
    assert_eq!(
        migrate.find_flag("non-interactive").unwrap().short,
        Some('n')
    );
}

#[test]
fn config_migrate_needs_no_runtime_and_is_not_api_allowed_like_config_set() {
    let migrate = cat().lookup(PATH).unwrap();
    let set = cat().lookup(&["config", "set"]).unwrap();
    assert!(!migrate.requires_runtime);
    assert_eq!(migrate.requires_runtime, set.requires_runtime);
    assert!(!migrate.api_allowed);
    assert_eq!(migrate.api_allowed, set.api_allowed);
}

#[test]
fn config_migrate_is_reachable_from_cli_and_tui_and_refused_by_api() {
    let cat = cat();
    assert!(cat.validate_for_frontend(FrontendKind::Cli, PATH).is_ok());
    assert!(cat.validate_for_frontend(FrontendKind::Tui, PATH).is_ok());
    let err = cat
        .validate_for_frontend(FrontendKind::Api, PATH)
        .expect_err("the API must not run config migrate");
    assert!(
        err.to_string().contains("config migrate"),
        "refusal names the command: {err}"
    );
}

#[test]
fn config_migrate_parses_every_flag_through_the_cli_parser() {
    let clap = cat().build_clap_command();
    let m = clap
        .try_get_matches_from([
            "awman",
            "config",
            "migrate",
            "--yes",
            "--dry-run",
            "--global",
            "--non-interactive",
        ])
        .expect("flags parse");
    let (_, config) = m.subcommand().unwrap();
    let (name, migrate) = config.subcommand().unwrap();
    assert_eq!(name, "migrate");
    for flag in ["yes", "dry-run", "global", "non-interactive"] {
        assert!(migrate.get_flag(flag), "--{flag} not set");
    }
}

#[test]
fn config_set_global_flag_still_exists_for_the_global_scope_writes() {
    let set = cat().lookup(&["config", "set"]).unwrap();
    assert!(set.find_flag("global").is_some());
}
