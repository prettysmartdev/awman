//! WI 0102 — catalogue coverage for the squad CLI/TUI/API projections.
//!
//! These checks deliberately assert the complete shape of the squad subtree.
//! The cross-frontend `parity_test` is catalogue-driven and therefore covers
//! all leaf commands automatically; this module pins the non-leaf `squad`
//! flags and the user-facing command inventory as well.

use awman::command::dispatch::catalogue::{ArgumentKind, CommandCatalogue, FlagDefault, FlagKind};

fn cat() -> &'static CommandCatalogue {
    CommandCatalogue::get()
}

fn flag_names(path: &[&str]) -> Vec<&'static str> {
    let mut names: Vec<_> = cat()
        .lookup(path)
        .unwrap_or_else(|| panic!("missing catalogue command {path:?}"))
        .flags
        .iter()
        .map(|flag| flag.long)
        .collect();
    names.sort_unstable();
    names
}

fn argument_names(path: &[&str]) -> Vec<&'static str> {
    let mut names: Vec<_> = cat()
        .lookup(path)
        .unwrap_or_else(|| panic!("missing catalogue command {path:?}"))
        .arguments
        .iter()
        .map(|argument| argument.name)
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn squad_is_a_top_level_command_with_its_root_flags() {
    let top_level: Vec<_> = cat()
        .root()
        .subcommands
        .iter()
        .map(|spec| spec.name)
        .collect();
    assert!(
        top_level.contains(&"squad"),
        "squad must be in the top-level catalogue"
    );

    assert_eq!(flag_names(&["squad"]), vec!["json", "non-interactive"]);
    assert!(cat()
        .lookup(&["squad"])
        .unwrap()
        .find_flag("json")
        .unwrap()
        .implies
        .contains(&"non-interactive"));

    let non_interactive = cat()
        .lookup(&["squad"])
        .unwrap()
        .find_flag("non-interactive")
        .unwrap();
    assert_eq!(non_interactive.short, Some('n'));
    assert!(matches!(non_interactive.kind, FlagKind::Bool));
}

#[test]
fn every_squad_subcommand_has_the_contract_shape() {
    let expected = [
        (
            "start",
            vec![],
            vec!["background", "dangerously-skip-auth", "port", "refresh-key"],
        ),
        ("stop", vec![], vec![]),
        ("status", vec![], vec!["json"]),
        ("logs", vec![], vec!["follow"]),
        (
            "add",
            vec![],
            vec![
                "agent",
                // WI 0110: the task-scoped agent pool, shared with `squad edit`.
                "agent-models",
                "description",
                "interval",
                "interview",
                "model",
                "mount-scope",
                "name",
                "non-interactive",
                // WI 0106: task overlays and the default-vs-custom workspace
                // choice are part of `squad add`'s scripted surface.
                "overlay",
                "repo",
                "workspace",
            ],
        ),
        // WI 0110: `squad edit` carries every field a task may change after
        // creation. `name` stays an argument; `workspace`/`mount-scope` are
        // absent because they are captured once and never edited.
        (
            "edit",
            vec!["name"],
            vec![
                "agent",
                "agent-models",
                "clear-agent",
                "clear-agent-models",
                "clear-model",
                "clear-overlays",
                "description",
                "interval",
                "interview",
                "model",
                "non-interactive",
                "overlay",
            ],
        ),
        ("list", vec![], vec!["json"]),
        ("show", vec!["name"], vec!["json"]),
        ("remove", vec!["name"], vec!["yes"]),
        ("pause", vec!["name"], vec![]),
        ("resume", vec!["name"], vec![]),
        // `trigger` carries no flags: an out-of-schedule evaluation has
        // nothing to configure, and it deliberately changes no stored field.
        ("trigger", vec!["name"], vec![]),
        ("attach", vec!["name"], vec!["container"]),
        // WI 0116 §6d: `squad env` reports coverage, pushes, and clears the
        // stored item. No short forms, no aliases, and neither flag implies
        // the other.
        ("env", vec![], vec!["clear", "json", "push"]),
    ];

    for (name, arguments, flags) in expected {
        let path = ["squad", name];
        let command = cat()
            .lookup(&path)
            .unwrap_or_else(|| panic!("missing squad command {name}"));
        assert_eq!(
            argument_names(&path),
            arguments,
            "arguments for squad {name}"
        );
        let mut expected_flags = flags;
        expected_flags.sort_unstable();
        assert_eq!(flag_names(&path), expected_flags, "flags for squad {name}");
        assert!(
            command.api_allowed || name == "attach" || name == "env",
            "unexpected API policy for squad {name}"
        );
    }

    let attach_name = cat().lookup(&["squad", "attach"]).unwrap().arguments[0];
    assert!(matches!(attach_name.kind, ArgumentKind::String));
}

#[test]
fn squad_start_does_not_reintroduce_the_retired_interval_flag() {
    assert!(cat()
        .lookup(&["squad", "start"])
        .unwrap()
        .find_flag("interval")
        .is_none());
}

#[test]
fn squad_flag_types_and_short_forms_are_projection_safe() {
    let add = cat().lookup(&["squad", "add"]).unwrap();
    assert!(matches!(
        add.find_flag("repo").unwrap().kind,
        FlagKind::Path
    ));
    assert!(matches!(
        add.find_flag("mount-scope").unwrap().kind,
        FlagKind::Enum(_)
    ));
    assert_eq!(add.find_flag("non-interactive").unwrap().short, Some('n'));

    let logs = cat().lookup(&["squad", "logs"]).unwrap();
    assert_eq!(logs.find_flag("follow").unwrap().short, Some('f'));

    let remove = cat().lookup(&["squad", "remove"]).unwrap();
    assert_eq!(remove.find_flag("yes").unwrap().short, Some('y'));
}

#[test]
fn squad_add_interval_defaults_to_six_hours_but_accepts_an_explicit_five_minutes() {
    let interval = cat()
        .lookup(&["squad", "add"])
        .unwrap()
        .find_flag("interval")
        .unwrap();
    assert!(matches!(interval.default, FlagDefault::Str("6h")));
    let explicit = cat()
        .build_clap_command()
        .try_get_matches_from([
            "awman",
            "squad",
            "add",
            "--name",
            "five-minutes",
            "--description",
            "explicit interval",
            "--interval",
            "5m",
        ])
        .expect("an explicit 5m interval must remain valid");
    let (_, subcommand) = explicit.subcommand().unwrap();
    let (_, add) = subcommand.subcommand().unwrap();
    assert_eq!(
        add.get_one::<String>("interval").map(String::as_str),
        Some("5m")
    );
}

/// WI 0116 §6d. `squad env` is deliberately **not** API-allowed: it reports on
/// this shell's own coverage relative to a daemon, which is a local question,
/// and admitting it through the `/v1/commands` front door would make the leaf
/// remotely reachable. Its `GatewayNeed` is `Running`, exactly like `squad
/// list`, so it starts a daemon when none is up rather than reporting "nothing
/// required" against a daemon that does not exist.
#[test]
fn squad_env_is_local_only_and_needs_a_running_daemon() {
    use awman::command::dispatch::catalogue::GatewayNeed;

    let env = cat().lookup(&["squad", "env"]).expect("squad env exists");
    assert!(
        !env.api_allowed,
        "`squad env` must not be reachable over the API"
    );
    assert!(matches!(env.gateway_need, GatewayNeed::Running));
    // Spelled in full, no short forms — neither flag may collide with, or
    // shadow, the other.
    for flag in ["push", "clear", "json"] {
        let flag = env.find_flag(flag).unwrap_or_else(|| panic!("--{flag}"));
        assert_eq!(flag.short, None, "`squad env` takes no short forms");
        assert!(matches!(flag.kind, FlagKind::Bool));
    }
}
