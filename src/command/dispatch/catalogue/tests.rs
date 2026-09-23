//! Tests for the command catalogue (WI 0114 F-51: moved out of the module
//! file, unchanged).

use super::*;

// ── suggest ──────────────────────────────────────────────────────────────

#[test]
fn suggest_corrects_a_top_level_typo() {
    let cat = CommandCatalogue::get();
    assert_eq!(
        cat.suggest(&["cht"]).first().map(String::as_str),
        Some("chat")
    );
}

/// The behaviour change WI 0114 F-43 buys: both frontends previously
/// searched only the top-level command list, so a mistyped subcommand got
/// no suggestion at all (or a nonsensical top-level one).
#[test]
fn suggest_corrects_a_nested_subcommand_against_its_own_parent() {
    let cat = CommandCatalogue::get();
    let suggestions = cat.suggest(&["exec", "wrkflow"]);
    assert_eq!(
        suggestions.first().map(String::as_str),
        Some("exec workflow"),
        "a nested typo must be corrected under the prefix that resolved, \
             and returned as a full path; got {suggestions:?}"
    );
}

#[test]
fn suggest_is_empty_for_an_unrecognisable_command() {
    let cat = CommandCatalogue::get();
    assert!(cat.suggest(&["zzzzzzzzzzzz"]).is_empty());
}

#[test]
fn suggest_is_empty_when_the_prefix_itself_does_not_resolve() {
    let cat = CommandCatalogue::get();
    assert!(
        cat.suggest(&["nosuchgroup", "workflow"]).is_empty(),
        "there is nothing sensible to search under an unknown parent"
    );
    assert!(cat.suggest(&[]).is_empty());
}

/// Walk every spec in the catalogue, root included, with its path.
fn all_specs() -> Vec<(Vec<&'static str>, &'static CommandSpec)> {
    fn walk(
        spec: &'static CommandSpec,
        path: Vec<&'static str>,
        out: &mut Vec<(Vec<&'static str>, &'static CommandSpec)>,
    ) {
        out.push((path.clone(), spec));
        for sub in spec.subcommands {
            let mut child = path.clone();
            child.push(sub.name);
            walk(sub, child, out);
        }
    }
    let mut out = Vec::new();
    walk(CommandCatalogue::get().root(), Vec::new(), &mut out);
    out
}

/// A `FlagDefault` of the wrong shape for its `FlagKind` would be silently
/// dropped when the flag is resolved (`dispatch::resolved::apply_default`
/// leaves the read value alone), so the mismatch is caught here instead.
#[test]
fn every_flag_default_matches_its_kind() {
    for (path, spec) in all_specs() {
        for flag in spec.flags {
            let ok = matches!(
                (flag.kind, flag.default),
                (_, FlagDefault::None)
                    | (FlagKind::Bool, FlagDefault::Bool(_))
                    | (
                        FlagKind::String | FlagKind::OptionalString | FlagKind::Enum(_),
                        FlagDefault::Str(_)
                    )
                    | (FlagKind::U16, FlagDefault::U16(_))
                    | (FlagKind::VecString, FlagDefault::EmptyVec)
            );
            assert!(
                ok,
                "{} --{}: default {:?} does not match kind {:?}",
                path.join(" "),
                flag.long,
                flag.default,
                flag.kind
            );
        }
        // An enum default must name one of the enum's own values.
        for flag in spec.flags {
            if let (FlagKind::Enum(values), FlagDefault::Str(default)) = (flag.kind, flag.default) {
                assert!(
                    values.contains(&default),
                    "{} --{}: default {default:?} is not one of {values:?}",
                    path.join(" "),
                    flag.long
                );
            }
        }
    }
}

#[test]
fn lookup_top_level_returns_spec() {
    let cat = CommandCatalogue::get();
    let spec = cat.lookup(&["init"]).expect("init must be present");
    assert_eq!(spec.name, "init");
}

#[test]
fn lookup_nested_returns_spec() {
    let cat = CommandCatalogue::get();
    let spec = cat
        .lookup(&["exec", "workflow"])
        .expect("exec workflow must be present");
    assert_eq!(spec.name, "workflow");
}

#[test]
fn lookup_unknown_returns_none() {
    let cat = CommandCatalogue::get();
    assert!(cat.lookup(&["bogus"]).is_none());
    assert!(cat.lookup(&["init", "bogus"]).is_none());
}

#[test]
fn string_alias_wf_resolves_to_workflow() {
    let cat = CommandCatalogue::get();
    let spec = cat.lookup(&["exec", "wf"]).unwrap();
    assert_eq!(spec.name, "workflow");
}

#[test]
fn ready_json_implies_non_interactive() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    let json_flag = ready.find_flag("json").unwrap();
    assert!(json_flag.implies.contains(&"non-interactive"));
}

#[test]
fn exec_workflow_yolo_implies_worktree() {
    let cat = CommandCatalogue::get();
    let exec_workflow = cat.lookup(&["exec", "workflow"]).unwrap();
    let yolo = exec_workflow.find_flag("yolo").unwrap();
    assert!(yolo.implies.contains(&"worktree"));
}

#[test]
fn exec_workflow_auto_implies_worktree() {
    let cat = CommandCatalogue::get();
    let exec_workflow = cat.lookup(&["exec", "workflow"]).unwrap();
    let auto = exec_workflow.find_flag("auto").unwrap();
    assert!(auto.implies.contains(&"worktree"));
}

#[test]
fn plan_and_yolo_are_mutually_exclusive_on_chat() {
    let cat = CommandCatalogue::get();
    let chat = cat.lookup(&["chat"]).unwrap();
    let plan = chat.find_flag("plan").unwrap();
    assert!(plan.conflicts_with("yolo"));
    let yolo = chat.find_flag("yolo").unwrap();
    assert!(yolo.conflicts_with("plan"));
}

#[test]
fn every_top_level_command_is_present() {
    let cat = CommandCatalogue::get();
    for name in [
        "init", "ready", "chat", "specs", "status", "config", "exec", "api", "remote", "new",
    ] {
        assert!(cat.lookup(&[name]).is_some(), "missing top-level '{name}'");
    }
}

#[test]
fn remote_exec_workflow_has_workflow_argument() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["remote", "exec", "workflow"]).unwrap();
    assert_eq!(wf.arguments.len(), 1);
    assert_eq!(wf.arguments[0].name, "workflow");
    assert!(matches!(wf.arguments[0].kind, ArgumentKind::Path));
}

#[test]
fn remote_exec_prompt_has_prompt_argument() {
    let cat = CommandCatalogue::get();
    let prompt = cat.lookup(&["remote", "exec", "prompt"]).unwrap();
    assert_eq!(prompt.arguments.len(), 1);
    assert_eq!(prompt.arguments[0].name, "prompt");
    // Greedy trailing positional so multi-word prompts join spec-driven.
    assert!(matches!(
        prompt.arguments[0].kind,
        ArgumentKind::TrailingVarArgs
    ));
}

// ─── Data-table tests ─────────────────────────────────────────────────────

/// Compact check for a single flag: path, flag name, whether it is a Bool,
/// and whether it is optional.  The `bool_expected` field avoids PartialEq
/// on `FlagKind` (which contains `&'static [&'static str]` slices).
struct FlagCheck {
    path: &'static [&'static str],
    flag: &'static str,
    is_bool: bool,
    is_optional: bool,
}

const FLAG_TABLE: &[FlagCheck] = &[
    FlagCheck {
        path: &["init"],
        flag: "agent",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["init"],
        flag: "aspec",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["ready"],
        flag: "refresh",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["ready"],
        flag: "build",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["ready"],
        flag: "no-cache",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["ready"],
        flag: "non-interactive",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["ready"],
        flag: "allow-docker",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["ready"],
        flag: "json",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "non-interactive",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "plan",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "yolo",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "auto",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "allow-docker",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "agent",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "model",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["chat"],
        flag: "overlay",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["exec", "workflow"],
        flag: "yolo",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["exec", "workflow"],
        flag: "auto",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["exec", "workflow"],
        flag: "worktree",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["exec", "workflow"],
        flag: "work-item",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["exec", "workflow"],
        flag: "plan",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["exec", "prompt"],
        flag: "yolo",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["exec", "prompt"],
        flag: "overlay",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["status"],
        flag: "watch",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["config", "set"],
        flag: "global",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["api", "start"],
        flag: "port",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["api", "start"],
        flag: "workdirs",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["api", "start"],
        flag: "background",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["api", "start"],
        flag: "refresh-key",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["api", "start"],
        flag: "dangerously-skip-auth",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["api", "start"],
        flag: "dangerously-skip-tls",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["remote", "exec", "workflow"],
        flag: "follow",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["remote", "exec", "workflow"],
        flag: "api-key",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["remote", "exec", "workflow"],
        flag: "remote-addr",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["remote", "session", "start"],
        flag: "api-key",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["remote", "session", "kill"],
        flag: "remote-addr",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["new", "workflow"],
        flag: "format",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["new", "workflow"],
        flag: "interview",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["new", "workflow"],
        flag: "global",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["new", "skill"],
        flag: "interview",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["new", "skill"],
        flag: "global",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["new", "spec"],
        flag: "interview",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["specs", "amend"],
        flag: "non-interactive",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["specs", "amend"],
        flag: "allow-docker",
        is_bool: true,
        is_optional: true,
    },
];

#[test]
fn all_documented_flags_present_with_correct_kind_and_optional() {
    let cat = CommandCatalogue::get();
    for case in FLAG_TABLE {
        let spec = cat
            .lookup(case.path)
            .unwrap_or_else(|| panic!("command {:?} not found in catalogue", case.path));
        let flag = spec
            .find_flag(case.flag)
            .unwrap_or_else(|| panic!("flag '{}' not found on {:?}", case.flag, case.path));
        assert_eq!(
            flag.optional, case.is_optional,
            "optional mismatch for '{}' on {:?}",
            case.flag, case.path
        );
        assert_eq!(
            matches!(flag.kind, FlagKind::Bool),
            case.is_bool,
            "is_bool mismatch for '{}' on {:?}",
            case.flag,
            case.path
        );
    }
}

#[test]
fn all_expected_subcommands_are_present() {
    let cat = CommandCatalogue::get();
    let cases: &[(&[&str], &str)] = &[
        (&["specs"], "amend"),
        (&["config"], "show"),
        (&["config"], "get"),
        (&["config"], "set"),
        (&["exec"], "prompt"),
        (&["exec"], "workflow"),
        (&["api"], "start"),
        (&["api"], "kill"),
        (&["api"], "logs"),
        (&["api"], "status"),
        (&["remote"], "exec"),
        (&["remote"], "session"),
        (&["remote", "exec"], "workflow"),
        (&["remote", "exec"], "prompt"),
        (&["remote", "session"], "start"),
        (&["remote", "session"], "kill"),
        (&["new"], "spec"),
        (&["new"], "workflow"),
        (&["new"], "skill"),
    ];
    for (parent_path, subcmd_name) in cases {
        let parent = cat
            .lookup(parent_path)
            .unwrap_or_else(|| panic!("parent {:?} not found", parent_path));
        assert!(
            parent.find_subcommand(subcmd_name).is_some(),
            "subcommand '{}' not found under {:?}",
            subcmd_name,
            parent_path
        );
    }
}

#[test]
fn config_commands_do_not_require_runtime() {
    let cat = CommandCatalogue::get();
    for path in [
        &["config"][..],
        &["config", "show"],
        &["config", "get"],
        &["config", "set"],
    ] {
        assert!(
            !cat.requires_runtime(path),
            "{path:?} must not require a runtime — it is the recovery \
                 path for a broken runtime config"
        );
    }
    // Everything else — and the bare TUI invocation (empty path) —
    // conservatively requires a detected runtime.
    for path in [
        &["chat"][..],
        &["ready"],
        &["init"],
        &["status"],
        &["exec", "workflow"],
        &[],
    ] {
        assert!(cat.requires_runtime(path), "{path:?} must require runtime");
    }
}

/// The `config` subtree is the whole of the runtime-optional surface.
///
/// `requires_runtime` used to be `!matches!(path.first(), Some(&"config"))` —
/// a command-name fact spelled outside the catalogue. It is a `CommandSpec`
/// attribute now (F-25 step 4), so this walks every spec in the tree and
/// asserts the set of `false` ones is exactly the `config` subtree: a new
/// command that declares `requires_runtime: false` without meaning to fails
/// here.
#[test]
fn only_the_config_subtree_declares_requires_runtime_false() {
    fn walk(spec: &'static CommandSpec, path: Vec<&'static str>, out: &mut Vec<String>) {
        if !spec.requires_runtime {
            out.push(path.join(" "));
        }
        for sub in spec.subcommands {
            let mut child = path.clone();
            child.push(sub.name);
            walk(sub, child, out);
        }
    }

    let mut runtime_optional = Vec::new();
    walk(
        CommandCatalogue::get().root(),
        Vec::new(),
        &mut runtime_optional,
    );
    runtime_optional.sort();
    assert_eq!(
        runtime_optional,
        vec!["config", "config get", "config set", "config show"],
    );
}

/// `squad` is the one command with a bare TUI form.
///
/// Folded out of the catalogue-level `TUI_WHEN_BARE` list into a `CommandSpec`
/// attribute in the same pass (midpoint finding 24). The `false` half matters
/// as much as the `true`: `awman squad list` must not open a tab.
#[test]
fn only_squad_opens_the_tui_when_bare() {
    fn walk(spec: &'static CommandSpec, path: Vec<&'static str>, out: &mut Vec<String>) {
        if spec.opens_tui_when_bare {
            out.push(path.join(" "));
        }
        for sub in spec.subcommands {
            let mut child = path.clone();
            child.push(sub.name);
            walk(sub, child, out);
        }
    }

    let mut tui_when_bare = Vec::new();
    walk(
        CommandCatalogue::get().root(),
        Vec::new(),
        &mut tui_when_bare,
    );
    assert_eq!(tui_when_bare, vec!["squad"]);
}

#[test]
fn flag_spec_conflicts_with_accessor_is_symmetric_on_chat() {
    let cat = CommandCatalogue::get();
    let chat = cat.lookup(&["chat"]).unwrap();
    let plan = chat.find_flag("plan").unwrap();
    let yolo = chat.find_flag("yolo").unwrap();
    assert!(plan.conflicts_with("yolo"), "plan must conflict with yolo");
    assert!(yolo.conflicts_with("plan"), "yolo must conflict with plan");
    assert!(
        !plan.conflicts_with("non-interactive"),
        "plan must NOT conflict with non-interactive"
    );
}

#[test]
fn api_start_flags_are_cli_only() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["api", "start"]).unwrap();
    for flag in start.flags {
        assert!(
            matches!(flag.frontends, FrontendVisibility::CliOnly),
            "api start flag '{}' must be CliOnly, got {:?}",
            flag.long,
            flag.frontends
        );
    }
}

#[test]
fn exec_workflow_arguments_include_workflow_path() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["exec", "workflow"]).unwrap();
    assert_eq!(wf.arguments.len(), 1);
    assert_eq!(wf.arguments[0].name, "workflow");
    assert!(matches!(wf.arguments[0].kind, ArgumentKind::Path));
}

#[test]
fn config_get_and_set_have_required_field_argument() {
    let cat = CommandCatalogue::get();
    let get = cat.lookup(&["config", "get"]).unwrap();
    assert_eq!(get.arguments.len(), 1);
    assert_eq!(get.arguments[0].name, "field");
    assert!(!get.arguments[0].optional);

    let set = cat.lookup(&["config", "set"]).unwrap();
    assert_eq!(set.arguments.len(), 2);
    let names: Vec<&str> = set.arguments.iter().map(|a| a.name).collect();
    assert!(names.contains(&"field") && names.contains(&"value"));
}

// ── WI-0098 Finding B: removed-flag migration hints ───────────────────────

#[test]
fn removed_flag_hint_returns_hint_for_mount_ssh() {
    let cat = CommandCatalogue::get();
    let hint = cat
        .removed_flag_hint(["chat", "--mount-ssh"])
        .expect("--mount-ssh must yield a migration hint");
    assert!(
        hint.starts_with("--mount-ssh has been removed."),
        "hint must name the removed flag; got: {hint}"
    );
    assert!(
        hint.contains("ssh()") || hint.contains("--overlay"),
        "hint must point at the `--overlay ssh()` replacement; got: {hint}"
    );
}

#[test]
fn removed_flag_hint_matches_value_form() {
    let cat = CommandCatalogue::get();
    // `--mount-ssh=x` (the `=`-bearing form) must be intercepted too.
    let hint = cat
        .removed_flag_hint(["chat", "--mount-ssh=x"])
        .expect("--mount-ssh=x must yield the same migration hint");
    assert!(hint.starts_with("--mount-ssh has been removed."));
}

#[test]
fn removed_flag_hint_none_for_live_flags() {
    let cat = CommandCatalogue::get();
    // Live flags and near-misses must not trigger a removed-flag hint.
    assert!(cat
        .removed_flag_hint(["chat", "--overlay", "ssh()"])
        .is_none());
    assert!(cat
        .removed_flag_hint(["chat", "--non-interactive"])
        .is_none());
    // A flag that merely contains the removed name as a substring must not match.
    assert!(cat
        .removed_flag_hint(["chat", "--mount-ssh-extra"])
        .is_none());
    assert!(cat.removed_flag_hint(Vec::<String>::new()).is_none());
}

#[test]
fn launch_mode_rejects_unknown_enum_value() {
    let cat = CommandCatalogue::get();
    let err = cat
        .parse_raw_args(
            &["exec", "prompt"],
            &["--launch-mode".to_string(), "bogus".to_string()],
        )
        .expect_err("an unrecognized launch mode must be rejected");

    match err {
        crate::command::error::CommandError::InvalidFlagValue {
            command,
            flag,
            reason,
        } => {
            assert_eq!(command, vec!["exec".to_string(), "prompt".to_string()]);
            assert_eq!(flag, "launch-mode");
            assert_eq!(reason, "'bogus' is not one of [\"stdio\", \"acp\"]");
        }
        other => panic!("expected InvalidFlagValue, got {other:?}"),
    }
}

/// Every squad subcommand that talks to the daemon declares the need, and
/// the three lifecycle commands declare none.
///
/// This is the guard on the root cause of F-04: two frontends each carried
/// a hand-written list of these names, and the lists had already drifted
/// apart from each other and from the catalogue. `start`, `stop`, and
/// `logs` are the exceptions on purpose — `start` *is* the daemon, while
/// `stop` and `logs` act on the process and its file. `attach` now uses a
/// running gateway through Dispatch (WI 0113 Step 10).
#[test]
fn every_squad_subcommand_declares_whether_it_needs_a_gateway() {
    const NO_GATEWAY: &[&str] = &["start", "stop", "logs"];
    let squad = CommandCatalogue::get()
        .lookup(&["squad"])
        .expect("squad must exist");
    assert!(
        !squad.subcommands.is_empty(),
        "the squad subtree must not be empty, or this test proves nothing"
    );
    for sub in squad.subcommands {
        if NO_GATEWAY.contains(&sub.name) {
            assert_eq!(
                sub.gateway_need,
                GatewayNeed::None,
                "`squad {}` must never try to acquire a gateway",
                sub.name
            );
        } else {
            assert_ne!(
                sub.gateway_need,
                GatewayNeed::None,
                "`squad {}` reaches the daemon and must declare a gateway need",
                sub.name
            );
        }
    }
}

/// `squad status` reports on a daemon rather than requiring one, so it must
/// never start one: with nothing running it still succeeds with a "not
/// running" summary.
#[test]
fn squad_status_asks_for_a_gateway_only_if_one_is_already_running() {
    let catalogue = CommandCatalogue::get();
    let status = catalogue
        .lookup(&["squad", "status"])
        .expect("squad status must exist");
    assert_eq!(status.gateway_need, GatewayNeed::IfRunning);
    let bare = catalogue.lookup(&["squad"]).expect("squad must exist");
    assert_eq!(bare.gateway_need, GatewayNeed::IfRunning);
}

#[test]
fn squad_attach_requires_a_running_gateway_and_is_excluded_from_the_api() {
    let attach = CommandCatalogue::get()
        .lookup(&["squad", "attach"])
        .expect("squad attach must exist");
    assert_eq!(attach.gateway_need, GatewayNeed::Running);
    assert!(attach.requires_container_tier);
    assert!(!attach.api_allowed);
}

/// The runtime-tier guard is catalogue-driven, and squad is the only
/// subtree that carries it: a sandbox-class runtime cannot mount task
/// directories or run workflow setup/teardown steps.
#[test]
fn the_container_tier_requirement_is_the_squad_subtree_and_nothing_else() {
    fn walk(spec: &'static CommandSpec, path: Vec<&'static str>, out: &mut Vec<Vec<&str>>) {
        if spec.requires_container_tier {
            out.push(path.clone());
        }
        for sub in spec.subcommands {
            let mut child = path.clone();
            child.push(sub.name);
            walk(sub, child, out);
        }
    }
    let mut tiered = Vec::new();
    walk(CommandCatalogue::get().root(), Vec::new(), &mut tiered);
    assert!(
        !tiered.is_empty(),
        "the squad subtree must carry the requirement"
    );
    for path in &tiered {
        assert_eq!(
            path.first(),
            Some(&"squad"),
            "only squad requires a container tier; found {path:?}"
        );
    }
}

#[test]
fn launch_mode_is_registered_on_each_local_agent_command() {
    let cat = CommandCatalogue::get();
    let paths: &[&[&str]] = &[&["chat"], &["exec", "prompt"], &["exec", "workflow"]];
    for path in paths {
        let command = cat.lookup(path).expect("agent command must exist");
        let flag = command
            .find_flag("launch-mode")
            .expect("agent command must expose --launch-mode");
        assert!(flag.optional);
        match flag.kind {
            FlagKind::Enum(values) => assert_eq!(values, &["stdio", "acp"]),
            other => panic!("expected enum flag, got {other:?}"),
        }
    }
}

// ─── Derived flag arrays (WI 0114 F-51) ─────────────────────────────────────

/// Every override names a flag that is actually in the base set. An override
/// for a flag that is not there is silently ignored by `override_flags`, which
/// would leave `exec workflow` quietly using the base flag instead.
#[test]
fn every_exec_workflow_override_replaces_a_base_flag() {
    for over in super::shared_flags::EXEC_WORKFLOW_BASE_OVERRIDES {
        assert!(
            super::shared_flags::AGENT_RUN_FLAGS_NO_WORKTREE
                .iter()
                .any(|base| base.long == over.long),
            "--{} overrides nothing in the base set",
            over.long
        );
    }
}

/// `exec prompt` is the base set plus `--issue`, in that order — what the
/// literal spelled out before it was derived.
#[test]
fn exec_prompt_flags_are_the_base_set_plus_issue() {
    let names: Vec<&str> = super::shared_flags::EXEC_PROMPT_FLAGS
        .iter()
        .map(|f| f.long)
        .collect();
    let base: Vec<&str> = super::shared_flags::AGENT_RUN_FLAGS_NO_WORKTREE
        .iter()
        .map(|f| f.long)
        .collect();
    assert_eq!(&names[..base.len()], &base[..]);
    assert_eq!(&names[base.len()..], &["issue"]);
}

/// `exec workflow`'s order is the one `--help` and the generated command
/// reference print, and the splice must not have moved anything.
#[test]
fn exec_workflow_flags_keep_their_documented_order() {
    let names: Vec<&str> = super::shared_flags::EXEC_WORKFLOW_FLAGS
        .iter()
        .map(|f| f.long)
        .collect();
    assert_eq!(
        names,
        vec![
            "work-item",
            "non-interactive",
            "plan",
            "allow-docker",
            "launch-mode",
            "worktree",
            "yolo",
            "auto",
            "agent",
            "model",
            "overlay",
            "issue",
            "dynamic",
            "leader",
            "max-concurrent",
        ]
    );
}
