//! The derived list is also the Layer 3 parity inventory (design 8.4).
use awman::command::dispatch::catalogue::{CommandCatalogue, CommandSpec, FrontendKind};
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
    "specs amend",
    "agents status",
    "agents list",
    "agents detail",
    "agents kill",
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

/// The worker may execute agents and workflow/squad operations. Local setup,
/// config, credentials, remote delegation and foreground terminals stay local.
/// An independent permission oracle: flipping api_allowed must turn this red.
const WORKER_COMMANDS: &[&str] = &[
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
fn authorization_is_checked_for_every_leaf_and_alias() {
    let cat = CommandCatalogue::get();
    for (path, spec) in leaves() {
        let worker_allowed = WORKER_COMMANDS.contains(&path.join(" ").as_str());
        assert_eq!(
            spec.api_allowed, worker_allowed,
            "review permission changes explicitly: {path:?}"
        );
        for (frontend, allowed) in [
            (FrontendKind::Cli, true),
            (FrontendKind::Tui, true),
            (FrontendKind::WORKER_LOCAL, worker_allowed),
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
        ]
    );
    // Remote derivation drops the local-only selectors and adds transport.
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
    ];
    assert_eq!(flag_names(&["exec", "workflow"]), expected);
    let remote: Vec<&str> = ["session", "follow"]
        .into_iter()
        .chain(
            expected
                .into_iter()
                .filter(|name| !["worktree", "json"].contains(name)),
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
