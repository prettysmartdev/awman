//! The derived list is also the Layer 3 parity inventory (design 8.4).
use awman::command::dispatch::catalogue::{
    ApiAvailability, CommandCatalogue, CommandSpec, FrontendKind,
};
use awman::command::error::CommandError;
use std::collections::BTreeSet;

pub fn leaves() -> Vec<(Vec<&'static str>, &'static CommandSpec)> {
    fn walk(
        spec: &'static CommandSpec,
        path: Vec<&'static str>,
        out: &mut Vec<(Vec<&'static str>, &'static CommandSpec)>,
    ) {
        if spec.subcommands.is_empty() {
            out.push((path.clone(), spec));
        }
        for child in spec.subcommands {
            let mut path = path.clone();
            path.push(child.name);
            walk(child, path, out);
        }
    }
    let mut out = vec![];
    for spec in CommandCatalogue::get().root().subcommands {
        walk(spec, vec![spec.name], &mut out);
    }
    out
}

/// Explicit review boundary: adding a command requires extending its contract.
/// Kept independent of the catalogue traversal so a missing scenario fails.
pub const CONTRACT_PATHS: &[&str] = &[
    "attach",
    "init",
    "ready",
    "chat",
    "edit spec",
    "edit workflow",
    "edit skill",
    "edit agent",
    "agents status",
    "agents list",
    "agents detail",
    "agents kill",
    "agents profile list",
    "agents profile delete",
    "agents profile close",
    "config show",
    "config get",
    "config set",
    "config unset",
    "exec prompt",
    "exec workflow",
    "daemon start",
    "daemon stop",
    "daemon status",
    "daemon logs",
    "daemon api",
    "squad add",
    "squad list",
    "squad show",
    "squad edit",
    "squad remove",
    "squad pause",
    "squad resume",
    "squad trigger",
    "squad cancel",
    "squad attach",
    "squad env",
    "remote exec workflow",
    "remote exec prompt",
    "remote session start",
    "remote session kill",
    "new spec",
    "new workflow",
    "new skill",
    "new agent",
    "clean",
    "view",
    "link",
    "queue pick",
];

/// The worker may execute agents, workflow/squad operations and environment
/// readiness/build checks. Local config, credentials, remote delegation and
/// foreground terminals stay local.
/// An independent permission oracle: flipping api_allowed must turn this red.
const WORKER_COMMANDS: &[&str] = &[
    "init",
    "ready",
    "agents status",
    "agents list",
    "agents detail",
    "agents kill",
    "exec prompt",
    "exec workflow",
    "squad add",
    "squad edit",
    "squad list",
    "squad show",
    "squad remove",
    "squad pause",
    "squad resume",
    "squad trigger",
    "squad cancel",
];

/// Owner-local commands (WI 0070): declared for the API, but only for the
/// authenticated local owner on the local listener. Until that admission is
/// wired they are refused for network, browser and relay callers, like a disabled
/// command. An independent oracle, like `WORKER_COMMANDS`.
const LOCAL_OWNER_COMMANDS: &[&str] = &[
    "agents profile list",
    "agents profile delete",
    "agents profile close",
];

/// Commands the API offers only as an owner-local automation terminal
/// (`--automation`, `--settings-profile`, interactive; WI 0070).
const LOCAL_AUTOMATION_TERMINAL_COMMANDS: &[&str] = &["chat"];

#[test]
fn every_catalogue_leaf_has_a_reviewed_command_contract() {
    let actual: BTreeSet<_> = leaves().iter().map(|(p, _)| p.join(" ")).collect();
    let expected: BTreeSet<_> = CONTRACT_PATHS.iter().map(|p| p.to_string()).collect();
    assert_eq!(
        expected.len(),
        CONTRACT_PATHS.len(),
        "duplicate contract registration"
    );
    assert_eq!(
        actual, expected,
        "add a contract scenario when adding a command"
    );
}

#[test]
fn api_init_accepts_agent_and_aspec_fields() {
    let catalogue = CommandCatalogue::get();
    let frontend = FrontendKind::WORKER_LOCAL;
    let path = ["init"];
    assert!(catalogue.validate_for_frontend(frontend, &path).is_ok());
    let parsed = catalogue
        .parse_raw_args_with_profile(&path, &args(&["--agent", "claude", "--aspec"]), frontend)
        .unwrap();
    assert_eq!(parsed.flag_string("agent").as_deref(), Some("claude"));
    assert_eq!(parsed.flag_bool("aspec"), Some(true));
}

#[test]
fn authorization_is_checked_for_every_leaf_and_alias() {
    let cat = CommandCatalogue::get();
    for (path, spec) in leaves() {
        let worker_allowed = WORKER_COMMANDS.contains(&path.join(" ").as_str());
        let expected = if worker_allowed {
            ApiAvailability::Standard
        } else if LOCAL_OWNER_COMMANDS.contains(&path.join(" ").as_str()) {
            ApiAvailability::LocalOwner
        } else if LOCAL_AUTOMATION_TERMINAL_COMMANDS.contains(&path.join(" ").as_str()) {
            ApiAvailability::LocalAutomationTerminal
        } else {
            ApiAvailability::Disabled
        };
        assert_eq!(
            spec.api_availability, expected,
            "review permission changes explicitly: {path:?}"
        );
        assert_eq!(cat.api_availability(&path), expected, "{path:?}");
        for (frontend, allowed) in [
            (FrontendKind::Cli, true),
            (FrontendKind::Tui, true),
            (
                FrontendKind::WORKER_LOCAL,
                worker_allowed
                    || LOCAL_OWNER_COMMANDS.contains(&path.join(" ").as_str())
                    || LOCAL_AUTOMATION_TERMINAL_COMMANDS.contains(&path.join(" ").as_str()),
            ),
            (
                FrontendKind::SQUAD_LOCAL,
                worker_allowed && path.first() == Some(&"squad"),
            ),
            (
                FrontendKind::Relayed {
                    origin: awman::command::dispatch::catalogue::RelayOrigin::Cli,
                },
                spec.relay_allowed,
            ),
        ] {
            let result = cat.validate_for_frontend(frontend, &path);
            assert_eq!(result.is_ok(), allowed, "{path:?} {frontend:?}");
            if !allowed {
                assert!(matches!(
                    result,
                    Err(CommandError::NotAvailableForFrontend { .. })
                ));
            }
        }
        for alias in spec.aliases {
            let mut alias_path = path.clone();
            *alias_path.last_mut().unwrap() = alias;
            assert_eq!(cat.canonical_path(&alias_path), path);
            assert_eq!(cat.requires_runtime(&alias_path), spec.requires_runtime);
        }
    }
    assert!(cat
        .validate_for_frontend(FrontendKind::WORKER_LOCAL, &["unknown"])
        .is_err());
}

#[test]
fn each_command_rejects_unknown_flags_before_execution() {
    let cat = CommandCatalogue::get();
    for (path, _) in leaves() {
        let error = cat
            .parse_raw_args_with_profile(&path, &["--wi0144-invalid".into()], FrontendKind::Cli)
            .unwrap_err();
        assert!(
            matches!(error, CommandError::UnknownFlag { .. }),
            "{path:?}: {error:?}"
        );
        assert_eq!(
            (error.exit_code(), error.http_status(), error.code()),
            (2, 400, "unknown_flag")
        );
    }
}

#[test]
fn generated_reference_covers_public_paths_and_hides_internal_commands() {
    let reference = CommandCatalogue::get().markdown_reference();
    assert_eq!(
        reference,
        include_str!("../../../docs/14-command-reference.md")
    );
    for (path, spec) in leaves() {
        assert_eq!(
            reference.contains(&format!("`awman {}`", path.join(" "))),
            !spec.internal,
            "{path:?}"
        );
    }
}

#[test]
fn lookup_resolves_every_leaf_and_refuses_unknown_paths() {
    let catalogue = CommandCatalogue::get();
    for path in CONTRACT_PATHS {
        let components: Vec<_> = path.split(' ').collect();
        let spec = catalogue
            .lookup(&components)
            .unwrap_or_else(|| panic!("missing leaf {path}"));
        assert!(spec.subcommands.is_empty(), "{path} resolved to a group");
    }
    assert!(catalogue.lookup(&["not-a-command"]).is_none());
    assert!(catalogue.lookup(&["config", "not-a-command"]).is_none());
}

// ── WI 0147: `--work-item` replaces `--issue` ───────────────────────────────

const ISSUE_REPLACEMENT: &str = "--issue was replaced by --work-item; use --work-item github:42";

/// The commands that name a work item with `--work-item`.
const WORK_ITEM_COMMANDS: &[&[&str]] = &[
    &["exec", "prompt"],
    &["exec", "workflow"],
    &["new", "spec"],
    &["remote", "exec", "prompt"],
    &["remote", "exec", "workflow"],
];

fn flag_names(path: &[&str]) -> Vec<&'static str> {
    CommandCatalogue::get()
        .lookup(path)
        .unwrap_or_else(|| panic!("missing {path:?}"))
        .flags
        .iter()
        .map(|flag| flag.long)
        .collect()
}

fn args(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| token.to_string()).collect()
}

fn replacement_reason(error: CommandError) -> String {
    match error {
        CommandError::InvalidFlagValue { flag, reason, .. } if flag == "issue" => reason,
        other => panic!("expected the --issue replacement error, got {other:?}"),
    }
}

#[test]
fn issue_flag_is_absent_from_every_command_and_work_item_is_shared() {
    let catalogue = CommandCatalogue::get();
    for (path, spec) in leaves() {
        assert!(
            spec.find_flag("issue").is_none(),
            "{path:?} still declares --issue"
        );
        for flag in spec.flags {
            assert!(
                !flag.conflicts_with.contains(&"issue"),
                "{path:?} --{} still conflicts with --issue",
                flag.long
            );
            if flag.long == "work-item" {
                assert!(flag.conflicts_with.is_empty(), "{path:?} --work-item");
            }
        }
    }
    let shared = catalogue
        .lookup(&["exec", "prompt"])
        .unwrap()
        .find_flag("work-item")
        .unwrap();
    for path in WORK_ITEM_COMMANDS {
        let flag = catalogue
            .lookup(path)
            .unwrap()
            .find_flag("work-item")
            .unwrap_or_else(|| panic!("{path:?} lacks --work-item"));
        assert_eq!(flag.help, shared.help, "{path:?}");
        assert!(flag.optional, "{path:?}");
    }
    for form in [
        "local:",
        "github:",
        "owner/repo#",
        "http:",
        "workItems.source",
    ] {
        assert!(shared.help.contains(form), "help must describe {form}");
    }
    // `remote exec prompt` may be a work item alone, like `exec prompt`.
    let parsed = catalogue
        .parse_raw_args(
            &["remote", "exec", "prompt"],
            &args(&["--work-item", "PROJ-7"]),
        )
        .unwrap();
    assert_eq!(parsed.flag_string("work-item").as_deref(), Some("PROJ-7"));
    assert_eq!(parsed.argument("prompt"), None);
}

#[test]
fn exec_prompt_flags_are_the_base_set_plus_work_item() {
    assert_eq!(
        flag_names(&["exec", "prompt"]),
        [
            "headless",
            "plan",
            "allow-docker",
            "yolo",
            "auto",
            "agent",
            "model",
            "overlay",
            "agent-flag",
            "work-item",
            "interactive",
            "branch",
            "pr",
            "worktree",
            "read-only-workspace",
            "settings-profile",
            "agent-session",
            "automation",
            "prerequisite-identity",
        ]
    );
    // Remote derivation drops the local-only selectors (worktree selectors and the
    // owner-local profile, native-session and automation flags) and adds transport.
    assert_eq!(
        flag_names(&["remote", "exec", "prompt"]),
        [
            "session",
            "follow",
            "headless",
            "plan",
            "allow-docker",
            "yolo",
            "auto",
            "agent",
            "model",
            "overlay",
            "agent-flag",
            "work-item",
            "interactive",
            "read-only-workspace",
            "prerequisite-identity",
        ]
    );
    // It composes with 0151's session-worktree selectors.
    let parsed = CommandCatalogue::get()
        .parse_raw_args(
            &["exec", "prompt"],
            &args(&[
                "--branch",
                "feature",
                "--work-item",
                "local:7",
                "review",
                "this",
            ]),
        )
        .unwrap();
    assert_eq!(parsed.flag_string("branch").as_deref(), Some("feature"));
    assert_eq!(parsed.flag_string("work-item").as_deref(), Some("local:7"));
    assert_eq!(parsed.argument("prompt").as_deref(), Some("review this"));
}

#[test]
fn exec_workflow_flags_keep_their_documented_order() {
    let expected = [
        "work-item",
        "headless",
        "plan",
        "allow-docker",
        "interactive",
        "worktree",
        "yolo",
        "auto",
        "agent",
        "model",
        "overlay",
        "agent-flag",
        "dynamic",
        "leader",
        "on-dirty-merge-target",
        "on-merge-failure",
        "on-existing-worktree",
        "prerequisite-identity",
        "prerequisite-ssh",
        "prerequisite-token",
        "prerequisite-unresolved",
        "check-prerequisites",
        "json",
        "read-only-workspace",
        "settings-profile",
    ];
    assert_eq!(flag_names(&["exec", "workflow"]), expected);
    let remote: Vec<&str> = ["session", "follow"]
        .into_iter()
        .chain(
            expected
                .into_iter()
                .filter(|name| !["worktree", "json", "settings-profile"].contains(name)),
        )
        .collect();
    assert_eq!(flag_names(&["remote", "exec", "workflow"]), remote);
}

#[test]
fn issue_flag_names_its_replacement_on_every_frontend() {
    let catalogue = CommandCatalogue::get();
    for path in WORK_ITEM_COMMANDS {
        for frontend in [
            FrontendKind::Cli,
            FrontendKind::Tui,
            FrontendKind::WORKER_LOCAL,
        ] {
            if catalogue.validate_for_frontend(frontend, path).is_err() {
                continue;
            }
            let error = catalogue
                .parse_raw_args_with_profile(path, &args(&["--issue", "42"]), frontend)
                .unwrap_err();
            assert_eq!(
                replacement_reason(error),
                ISSUE_REPLACEMENT,
                "{path:?} {frontend:?}"
            );
        }
    }
    // The suggestion carries the value: a bare number or `#N` becomes
    // `github:N`; an explicit form is already valid as written.
    for (value, suggestion) in [
        ("--issue=#7", "github:7"),
        ("--issue=owner/repo#3", "owner/repo#3"),
        (
            "--issue=https://github.com/o/r/issues/9",
            "https://github.com/o/r/issues/9",
        ),
        ("--issue", "github:42"),
    ] {
        let error = catalogue
            .parse_raw_args(&["exec", "workflow"], &args(&[value]))
            .unwrap_err();
        assert_eq!(
            replacement_reason(error),
            format!("--issue was replaced by --work-item; use --work-item {suggestion}")
        );
    }
    // The TUI command box goes through the same parser.
    let error = awman::command::dispatch::parsed_input::parse("new spec --issue 42", catalogue)
        .unwrap_err();
    assert_eq!(replacement_reason(error), ISSUE_REPLACEMENT);
    // The CLI reports it before clap, global flags and all.
    assert_eq!(
        catalogue
            .removed_flag_hint([
                "awman", "--remote", "host", "remote", "exec", "workflow", "--issue", "42"
            ])
            .as_deref(),
        Some(ISSUE_REPLACEMENT)
    );
}

#[test]
fn issue_inside_prompt_text_is_not_a_flag() {
    let catalogue = CommandCatalogue::get();
    for tokens in [
        &["--", "mention", "--issue", "42"][..],
        &["fix", "--issue", "42"][..],
        &["fix --issue 42"][..],
    ] {
        let parsed = catalogue
            .parse_raw_args(&["exec", "prompt"], &args(tokens))
            .unwrap();
        assert!(
            parsed.argument("prompt").unwrap().contains("--issue"),
            "{tokens:?}"
        );
        let argv: Vec<&str> = ["awman", "exec", "prompt"]
            .iter()
            .chain(tokens)
            .copied()
            .collect();
        assert_eq!(catalogue.removed_flag_hint(argv), None, "{tokens:?}");
    }
}

#[test]
fn remote_exec_forwards_the_work_item_unparsed() {
    use awman::engine::remote::client::ExecArg;
    let forwarded = vec!["--work-item=PROJ-12".to_string()];
    let request = ExecArg::Prompt(None).into_request(&forwarded);
    assert_eq!(request.subcommand, "exec prompt");
    assert_eq!(
        request.args, forwarded,
        "no prompt positional without prompt text"
    );
    let request = ExecArg::Prompt(Some("go".into())).into_request(&forwarded);
    assert_eq!(request.args, ["--work-item=PROJ-12", "--", "go"]);
}

// ── WI 0059: `edit` replaces the removed `specs` command ───────────────────

fn flag(path: &[&str], long: &str) -> &'static awman::command::dispatch::catalogue::FlagSpec {
    CommandCatalogue::get()
        .lookup(path)
        .unwrap_or_else(|| panic!("missing {path:?}"))
        .flags
        .iter()
        .find(|f| f.long == long)
        .unwrap_or_else(|| panic!("{path:?} has no --{long}"))
}

#[test]
fn edit_has_four_leaves_and_every_new_kind_has_an_edit_counterpart() {
    let catalogue = CommandCatalogue::get();
    let edit = catalogue.lookup(&["edit"]).expect("edit group");
    let edit_kinds: BTreeSet<_> = edit.subcommands.iter().map(|s| s.name).collect();
    assert_eq!(
        edit_kinds,
        BTreeSet::from(["spec", "workflow", "skill", "agent"])
    );
    for leaf in edit.subcommands {
        assert!(leaf.subcommands.is_empty(), "edit {} is a leaf", leaf.name);
    }
    let new = catalogue.lookup(&["new"]).expect("new group");
    for kind in new.subcommands {
        assert!(
            edit_kinds.contains(kind.name),
            "`new {}` has no `edit {}`",
            kind.name,
            kind.name
        );
    }
}

#[test]
fn the_specs_command_is_gone_with_no_alias() {
    let catalogue = CommandCatalogue::get();
    assert!(catalogue.lookup(&["specs"]).is_none());
    assert!(catalogue.lookup(&["specs", "amend"]).is_none());
    assert!(catalogue.lookup_with_aliases(&["specs", "amend"]).is_none());
    assert!(leaves().iter().all(|(path, _)| path[0] != "specs"));
}

#[test]
fn authoring_has_no_interview_switch_and_keeps_headless_json_and_yolo() {
    for verb in ["new", "edit"] {
        for kind in ["spec", "workflow", "skill", "agent"] {
            let path = [verb, kind];
            let spec = CommandCatalogue::get().lookup(&path).unwrap();
            assert!(spec.find_flag("interview").is_none());
            for name in ["headless", "json", "yolo"] {
                assert!(spec.find_flag(name).is_some(), "{verb} {kind}: {name}");
            }
            assert!(CommandCatalogue::get().requires_runtime(&path));
            assert_eq!(flag(&path, "json").implies, &["headless"]);
        }
    }
    assert!(flag_names(&["edit", "spec"]).contains(&"amend"));
    assert!(flag_names(&["new", "skill"]).contains(&"pull"));
}

#[test]
fn work_item_short_alias_parses_on_every_supporting_command() {
    let catalogue = CommandCatalogue::get();
    let mut supported = Vec::new();
    for (path, spec) in leaves() {
        let Some(flag) = spec.find_flag("work-item") else {
            continue;
        };
        supported.push(path.join(" "));
        assert_eq!(flag.short, Some('w'), "{path:?}");
        assert_eq!(
            spec.flags.iter().filter(|f| f.short == Some('w')).count(),
            1
        );
        for spelling in [vec!["-w", "github:42"], vec!["--work-item", "github:42"]] {
            let parsed = catalogue.parse_raw_args(&path, &args(&spelling)).unwrap();
            assert_eq!(
                parsed.flag_string("work-item").as_deref(),
                Some("github:42"),
                "{path:?}"
            );
            let mut argv = vec!["awman"];
            argv.extend_from_slice(&path);
            argv.extend_from_slice(&spelling);
            let matches = catalogue
                .build_clap_command()
                .try_get_matches_from(argv)
                .unwrap();
            let mut leaf = &matches;
            for command in &path {
                leaf = leaf.subcommand_matches(command).unwrap();
            }
            assert_eq!(
                leaf.get_one::<String>("work-item").map(String::as_str),
                Some("github:42")
            );
        }
    }
    assert_eq!(
        supported.into_iter().collect::<BTreeSet<_>>(),
        WORK_ITEM_COMMANDS.iter().map(|p| p.join(" ")).collect()
    );
}

/// WI 0070: `--plan` is a native starting mode only. It conflicts with an
/// explicit `--yolo` / `--auto` on every launch path (so the API's default
/// `--yolo` is not applied beside it) and has no edge to
/// `--read-only-workspace` in either direction.
#[test]
fn plan_conflicts_with_yolo_and_auto_and_never_touches_mount_access() {
    let catalogue = CommandCatalogue::get();
    for path in [
        &["chat"][..],
        &["exec", "prompt"][..],
        &["exec", "workflow"][..],
        &["remote", "exec", "prompt"][..],
        &["remote", "exec", "workflow"][..],
    ] {
        let spec = catalogue.lookup(path).unwrap();
        let plan = spec.find_flag("plan").unwrap();
        assert_eq!(plan.conflicts_with, &["yolo", "auto"], "{path:?}");
        assert!(plan.implies.is_empty(), "{path:?}");
        assert!(
            !plan.help.contains("(read-only)"),
            "{path:?}: {}",
            plan.help
        );
        assert!(
            plan.help.contains("Does not change workspace mounts"),
            "{path:?}"
        );
        assert!(
            spec.find_flag("auto")
                .unwrap()
                .conflicts_with
                .contains(&"plan"),
            "{path:?}"
        );
        assert!(
            spec.find_flag("yolo")
                .unwrap()
                .conflicts_with
                .contains(&"plan"),
            "{path:?}"
        );
        if let Some(ro) = spec.find_flag("read-only-workspace") {
            assert!(
                !ro.conflicts_with.contains(&"plan") && ro.implies.is_empty(),
                "{path:?}"
            );
        }
        for mode in ["--yolo", "--auto"] {
            assert!(
                catalogue
                    .parse_raw_args(path, &args(&["--plan", mode]))
                    .is_err(),
                "{path:?} accepted --plan {mode}"
            );
        }
    }
}
