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
fn ready_json_implies_headless() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    let json_flag = ready.find_flag("json").unwrap();
    assert!(json_flag.implies.contains(&"headless"));
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
        "init", "ready", "chat", "specs", "agents", "config", "exec", "daemon", "squad", "remote",
        "new",
    ] {
        assert!(cat.lookup(&[name]).is_some(), "missing top-level '{name}'");
    }
}

#[test]
fn agents_has_exactly_the_four_supported_leaves() {
    let agents = CommandCatalogue::get().lookup(&["agents"]).unwrap();
    let mut leaves: Vec<_> = agents.subcommands.iter().map(|spec| spec.name).collect();
    leaves.sort_unstable();
    assert_eq!(leaves, ["detail", "kill", "list", "status"]);
}

#[test]
fn agent_short_flags_have_unique_meanings_across_catalogue() {
    fn walk(spec: &'static CommandSpec, path: Vec<&'static str>, found: &mut Vec<(char, String)>) {
        for flag in spec.flags {
            if let Some(short) = flag.short {
                found.push((short, format!("{} --{}", path.join(" "), flag.long)));
            }
        }
        for child in spec.subcommands {
            let mut child_path = path.clone();
            child_path.push(child.name);
            walk(child, child_path, found);
        }
    }
    let mut flags = Vec::new();
    walk(CommandCatalogue::get().root(), Vec::new(), &mut flags);
    for short in ['a', 'q', 'y'] {
        let meanings: Vec<_> = flags
            .iter()
            .filter(|(candidate, _)| *candidate == short)
            .map(|(_, name)| name.as_str())
            .collect();
        assert!(!meanings.is_empty(), "-{short} must be assigned");
        if short != 'y' {
            assert_eq!(
                meanings.len(),
                1,
                "-{short} must have one catalogue meaning"
            );
        } else {
            assert!(
                meanings.iter().all(|name| name.ends_with("--yes")),
                "-y is reserved for --yes: {meanings:?}"
            );
            for (_, path_flag) in flags.iter().filter(|(candidate, _)| *candidate == short) {
                let path = path_flag.rsplit_once(" --").unwrap().0;
                let leaf = CommandCatalogue::get()
                    .lookup(&path.split_whitespace().collect::<Vec<_>>())
                    .unwrap();
                assert!(
                    [
                        "Skip the confirmation prompt (for scripting).",
                        "Do not prompt for confirmation.",
                    ]
                    .contains(&leaf.find_flag("yes").unwrap().help),
                    "-y consistently means skip confirmation"
                );
            }
        }
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
        flag: "headless",
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
        flag: "headless",
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
        path: &["agents", "status"],
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
        path: &["config", "set"],
        flag: "repo",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["config", "unset"],
        flag: "task",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["daemon", "start"],
        flag: "background",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["daemon", "stop"],
        flag: "now",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["daemon", "status"],
        flag: "json",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["daemon", "logs"],
        flag: "follow",
        is_bool: true,
        is_optional: true,
    },
    FlagCheck {
        path: &["daemon", "api"],
        flag: "raw",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["daemon", "api"],
        flag: "data",
        is_bool: false,
        is_optional: true,
    },
    FlagCheck {
        path: &["daemon", "api"],
        flag: "list",
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
        flag: "headless",
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
        (&["config"], "unset"),
        (&["exec"], "prompt"),
        (&["exec"], "workflow"),
        (&["daemon"], "start"),
        (&["daemon"], "stop"),
        (&["daemon"], "status"),
        (&["daemon"], "logs"),
        (&["daemon"], "api"),
        (&["remote"], "exec"),
        (&["remote"], "session"),
        (&["remote", "exec"], "workflow"),
        (&["remote", "exec"], "prompt"),
        (&["remote", "session"], "start"),
        (&["remote", "session"], "kill"),
        (&["new"], "spec"),
        (&["new"], "workflow"),
        (&["new"], "skill"),
        (&["new"], "agent"),
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
        &["config", "unset"],
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
        &["agents", "status"],
        &["exec", "workflow"],
        &[],
    ] {
        assert!(cat.requires_runtime(path), "{path:?} must require runtime");
    }
}

/// The `config` subtree and `new agent` are the whole of the runtime-optional
/// surface.
///
/// `requires_runtime` used to be `!matches!(path.first(), Some(&"config"))` —
/// a command-name fact spelled outside the catalogue. It is a `CommandSpec`
/// attribute now (F-25 step 4), so this walks every spec in the tree and
/// asserts the set of `false` ones is exactly that list: a new
/// command that declares `requires_runtime: false` without meaning to fails
/// here.
#[test]
fn config_and_transport_clients_can_run_without_a_local_runtime() {
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
        vec![
            "attach",
            "config",
            "config get",
            "config set",
            "config show",
            "config unset",
            "daemon",
            "daemon api",
            "daemon logs",
            "daemon start",
            "daemon status",
            "daemon stop",
            // Writes a definition file and never builds.
            "new agent",
            "remote",
            "remote exec",
            "remote exec prompt",
            "remote exec workflow",
            "remote session",
            "remote session kill",
            "remote session start",
        ],
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
        !plan.conflicts_with("headless"),
        "plan must NOT conflict with headless"
    );
}

/// WI 0140: `awman daemon` has exactly five verbs; the `api` group and the
/// squad lifecycle verbs are gone.
#[test]
fn daemon_has_exactly_five_verbs_and_the_old_lifecycles_are_gone() {
    let cat = CommandCatalogue::get();
    let daemon = cat.lookup(&["daemon"]).unwrap();
    let verbs: Vec<_> = daemon.subcommands.iter().map(|spec| spec.name).collect();
    assert_eq!(verbs, ["start", "stop", "status", "logs", "api"]);
    for verb in &verbs {
        let spec = cat.lookup(&["daemon", verb]).unwrap();
        assert!(!spec.api_allowed, "daemon {verb} is local-only");
        assert_eq!(spec.gateway_need, GatewayNeed::None, "daemon {verb}");
        assert!(
            !spec.requires_runtime,
            "daemon {verb} needs no container runtime"
        );
    }
    assert!(cat.lookup(&["api"]).is_none(), "the api group is deleted");
    for gone in ["start", "stop", "status", "logs"] {
        assert!(
            cat.lookup(&["squad", gone]).is_none(),
            "squad {gone} moved to awman daemon"
        );
    }
    let clap = cat.build_clap_command();
    for argv in [
        &["awman", "api", "start"][..],
        &["awman", "squad", "start"],
        &["awman", "daemon", "set"],
        &["awman", "config", "prerequisite-grants", "list"],
    ] {
        assert!(
            clap.clone().try_get_matches_from(argv).is_err(),
            "{argv:?} must not parse"
        );
    }
    for argv in [
        &["awman", "daemon", "start", "--background"][..],
        &["awman", "daemon", "stop", "--now"],
        &["awman", "daemon", "status", "--json"],
        &["awman", "daemon", "logs", "-f"],
        &["awman", "daemon", "api", "--list"],
        &["awman", "daemon", "api", "keys.create", "name=ci"],
        &["awman", "daemon", "api", "--raw", "GET", "/v1/status"],
        &[
            "awman", "daemon", "api", "--raw", "POST", "/v1/keys", "--data", "-",
        ],
    ] {
        clap.clone()
            .try_get_matches_from(argv)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
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
fn config_get_and_set_have_required_key_argument() {
    let cat = CommandCatalogue::get();
    let get = cat.lookup(&["config", "get"]).unwrap();
    assert_eq!(get.arguments.len(), 1);
    assert_eq!(get.arguments[0].name, "key");
    assert!(!get.arguments[0].optional);

    let set = cat.lookup(&["config", "set"]).unwrap();
    assert_eq!(set.arguments.len(), 2);
    let names: Vec<&str> = set.arguments.iter().map(|a| a.name).collect();
    assert!(names.contains(&"key") && names.contains(&"value"));
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
    assert!(cat.removed_flag_hint(["chat", "--headless"]).is_none());
    // A flag that merely contains the removed name as a substring must not match.
    assert!(cat
        .removed_flag_hint(["chat", "--mount-ssh-extra"])
        .is_none());
    assert!(cat.removed_flag_hint(Vec::<String>::new()).is_none());
}

/// Every squad subcommand talks to the daemon, so every one declares the need.
///
/// This is the guard on the root cause of F-04: two frontends each carried
/// a hand-written list of these names, and the lists had already drifted
/// apart from each other and from the catalogue. `start`, `stop`, and
/// `logs` are the exceptions on purpose — `start` *is* the daemon, while
/// `stop` and `logs` act on the process and its file. `attach` now uses a
/// running gateway through Dispatch (WI 0113 Step 10).
#[test]
fn every_squad_subcommand_declares_whether_it_needs_a_gateway() {
    const NO_GATEWAY: &[&str] = &[];
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

/// Bare `squad --headless` reports on the daemon rather than requiring one:
/// with nothing running it still succeeds with a "not running" summary.
#[test]
fn bare_squad_asks_for_a_gateway_only_if_one_is_already_running() {
    let catalogue = CommandCatalogue::get();
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

/// `exec prompt` is the base set plus `--issue` and session-worktree flags.
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
    assert_eq!(
        &names[base.len()..],
        &["issue", "interactive", "branch", "pr", "worktree"]
    );
}

#[test]
fn session_worktree_flags_are_local_and_pairwise_conflicting() {
    let cat = CommandCatalogue::get();
    let worktree_names = ["branch", "pr", "worktree"];
    for path in [&["chat"][..], &["exec", "prompt"][..]] {
        let command = cat.lookup(path).unwrap();
        for name in worktree_names {
            let flag = command.find_flag(name).unwrap();
            assert!(matches!(flag.kind, FlagKind::OptionalString));
            assert_eq!(flag.frontends, FrontendVisibility::CliAndTui);
            for other in worktree_names {
                if name != other {
                    assert!(
                        flag.conflicts_with(other),
                        "{name} must conflict with {other}"
                    );
                }
            }
        }
    }
    let workflow = cat.lookup(&["exec", "workflow"]).unwrap();
    assert!(matches!(
        workflow.find_flag("worktree").unwrap().kind,
        FlagKind::Bool
    ));
    assert!(workflow.find_flag("branch").is_none());
    assert!(workflow.find_flag("pr").is_none());
    let remote_prompt = cat.lookup(&["remote", "exec", "prompt"]).unwrap();
    for name in worktree_names {
        assert!(remote_prompt.find_flag(name).is_none());
    }
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
            "issue",
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
        ]
    );
}

#[test]
fn existing_worktree_policy_flag_is_api_allowed_and_has_all_choices() {
    let cat = CommandCatalogue::get();
    let workflow = cat.lookup(&["exec", "workflow"]).unwrap();
    let flag = workflow
        .flags
        .iter()
        .find(|flag| flag.long == "on-existing-worktree")
        .expect("exec workflow exposes on-existing-worktree");
    assert!(
        matches!(flag.kind, FlagKind::Enum(values) if values == ["ask", "resume", "recreate", "fail"])
    );
    assert!(cat.is_allowed_for_frontend(FrontendKind::WORKER_LOCAL, &["exec", "workflow"]));
}

#[test]
fn agents_sibling_names_and_aliases_are_unique_and_root_status_is_absent() {
    let cat = CommandCatalogue::get();
    let agents = cat.lookup(&["agents"]).unwrap();
    let mut seen = std::collections::HashSet::new();
    for leaf in agents.subcommands {
        for name in std::iter::once(leaf.name).chain(leaf.aliases.iter().copied()) {
            assert!(seen.insert(name), "duplicate agents sibling: {name}");
        }
    }
    for child in cat.root().subcommands {
        assert_ne!(child.name, "status");
        assert!(!child.aliases.contains(&"status"));
    }
}

#[test]
fn attach_catalogue_replaces_agents_attach() {
    let cat = CommandCatalogue::get();
    assert!(cat.is_agent_attach(&["agents", "attach"]));
    assert!(cat.lookup(&["agents", "attach"]).is_none());
    assert!(cat.lookup(&["attach"]).is_some());
    assert!(!cat.is_agent_attach(&["agents"]));
    assert!(!cat.is_agent_attach(&["agents", "status"]));
    assert!(!cat.is_agent_attach(&["squad", "attach"]));
    assert!(!cat.is_agent_attach(&["agents", "attachx"]));
    assert!(!cat.is_agent_attach::<&str>(&[]));
    assert!(cat.is_terminal_attach(&["attach", "term_123"]));
    assert!(cat.is_terminal_attach(&["squad", "attach", "task_123"]));
}

#[test]
fn agents_docs_describe_current_scope_and_detach_controls() {
    let runtimes = include_str!("../../../../docs/11-runtimes.md");
    assert!(runtimes.contains("on the configured runtime"));
    assert!(!runtimes.contains("agents across all runtimes"));
    assert!(!runtimes.contains("show sandbox running/stopped state"));
    assert!(runtimes.contains("agents list\x60 never samples resource metrics"));
    let sessions = include_str!("../../../../docs/03-agent-sessions.md");
    assert!(sessions.contains("**Ctrl-C** to detach"));
    assert!(sessions.contains("--json --yes"));
    let list = CommandCatalogue::get().lookup(&["agents", "list"]).unwrap();
    assert_eq!(
        list.flags
            .iter()
            .find(|flag| flag.long == "no-trunc")
            .unwrap()
            .help,
        "Do not truncate IDs, names or image names."
    );
    let api = include_str!("../../../../docs/09-api-and-remote-mode.md");
    assert!(api.contains("across the configured runtime, independent of the selected"));
    assert!(api.contains("one \x60StdoutLine\x60 event"));
    let architecture = include_str!("../../../../docs/architecture.md");
    assert!(!architecture.contains("status.rs           StatusCommand"));
    assert!(architecture.contains("`agents/`"));
}

#[test]
fn new_agent_is_registered_writes_files_only_and_is_not_api_allowed() {
    let cat = CommandCatalogue::get();
    let spec = cat
        .lookup(&["new", "agent"])
        .expect("`new agent` is registered");
    assert!(!spec.api_allowed);
    assert!(!spec.requires_runtime);
    assert_eq!(spec.arguments.len(), 1);
    assert_eq!(spec.arguments[0].name, "name");
    assert!(!spec.arguments[0].optional);
    match spec.find_flag("from").expect("--from").kind {
        FlagKind::Enum(values) => assert_eq!(values, &["crush", "maki"]),
        other => panic!("--from should be an Enum, got {other:?}"),
    }
    assert!(spec.find_flag("global").is_some());
    assert!(spec.find_flag("force").is_some());
}

#[test]
fn init_agent_is_a_string_and_ready_takes_agent() {
    let cat = CommandCatalogue::get();
    let init = cat.lookup(&["init"]).unwrap().find_flag("agent").unwrap();
    assert!(matches!(init.kind, FlagKind::String));
    let ready = cat.lookup(&["ready"]).unwrap();
    assert!(matches!(
        ready.find_flag("agent").expect("ready --agent").kind,
        FlagKind::OptionalString
    ));
}

#[test]
fn init_agent_help_names_first_party_and_custom_agents_without_a_fixed_value_list() {
    let init = CommandCatalogue::get()
        .lookup(&["init"])
        .unwrap()
        .find_flag("agent")
        .unwrap();
    assert!(matches!(init.kind, FlagKind::String));
    for name in crate::engine::agent::first_party::FIRST_PARTY_AGENTS {
        assert!(init.help.contains(name), "init --agent help omits {name}");
    }
    assert!(init.help.contains(".awman/agents/"));
    for src in [
        include_str!("core.rs"),
        include_str!("mod.rs"),
        include_str!("new.rs"),
        include_str!("../../commands/init.rs"),
    ] {
        assert!(!src.contains(concat!("AGENT", "_VALUES")));
    }
}

#[test]
fn new_agent_help_documents_every_flag_and_the_name_rule() {
    let spec = CommandCatalogue::get().lookup(&["new", "agent"]).unwrap();
    assert!(!spec.help.is_empty());
    for flag in ["global", "from", "force"] {
        assert!(!spec.find_flag(flag).unwrap().help.is_empty(), "{flag}");
    }
}

#[test]
fn agent_tips_name_real_commands_and_stay_short() {
    use crate::command::commands::agents::tips::TIPS;
    let tips: Vec<&&str> = TIPS
        .iter()
        .filter(|t| t.contains("awman new agent") || t.contains("--pull builtin"))
        .collect();
    assert_eq!(tips.len(), 3, "{tips:?}");
    let cat = CommandCatalogue::get();
    assert!(cat.lookup(&["new", "agent"]).is_some());
    assert!(cat
        .lookup(&["new", "skill"])
        .unwrap()
        .find_flag("pull")
        .is_some());
    for t in tips {
        assert!(t.chars().count() <= 100, "{t}");
    }
}

#[test]
fn committed_command_reference_documents_new_agent() {
    let reference = include_str!("../../../../docs/14-command-reference.md");
    assert!(reference.contains("awman new agent"));
    assert!(reference.contains("--from"));
    assert!(reference.contains("awman ready"));
}

// ── session worktree selectors (WI 0153) ─────────────────────────────────

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// The selectors are available to the local frontends and refused by every
/// remote profile — including a `POST /v1/worker/commands` style `exec prompt`
/// request, which is parsed by the same profile parser the push routes will
/// use once they exist.
#[test]
fn session_worktree_selectors_are_refused_for_every_remote_profile() {
    let cat = CommandCatalogue::get();
    for path in [&["chat"][..], &["exec", "prompt"][..]] {
        for selector in ["--branch", "--pr", "--worktree"] {
            let mut args = argv(&[selector, "value"]);
            if path.len() == 2 {
                args.push("prompt text".into());
            }
            for local in [FrontendKind::Cli, FrontendKind::Tui] {
                let parsed = cat
                    .parse_raw_args_with_profile(path, &args, local)
                    .unwrap_or_else(|e| panic!("{path:?} {selector} on {local:?}: {e}"));
                assert_eq!(parsed.flag_string(&selector[2..]).as_deref(), Some("value"));
            }
            for remote in [FrontendKind::WORKER_LOCAL, FrontendKind::SQUAD_LOCAL] {
                let error = cat
                    .parse_raw_args_with_profile(path, &args, remote)
                    .expect_err(&format!(
                        "{path:?} {selector} must be refused on {remote:?}"
                    ))
                    .to_string();
                assert!(error.contains(&selector[2..]), "{error}");
            }
        }
    }
}

#[test]
fn worker_style_exec_prompt_request_with_selectors_is_refused() {
    let cat = CommandCatalogue::get();
    let request = argv(&["--pr", "42", "review", "this"]);
    assert!(cat
        .parse_raw_args_with_profile(&["exec", "prompt"], &request, FrontendKind::WORKER_LOCAL)
        .is_err());
    // The same prompt without the selector parses, so the refusal is the flag.
    let plain = argv(&["review", "this"]);
    let parsed = cat
        .parse_raw_args_with_profile(&["exec", "prompt"], &plain, FrontendKind::WORKER_LOCAL)
        .unwrap();
    assert_eq!(parsed.argument("prompt").as_deref(), Some("review this"));
    assert_eq!(parsed.flag_string("pr"), None);
}

#[test]
fn session_worktree_selectors_conflict_when_parsed_together() {
    let cat = CommandCatalogue::get();
    for path in [&["chat"][..], &["exec", "prompt"][..]] {
        for (a, b) in [
            ("--branch", "--pr"),
            ("--branch", "--worktree"),
            ("--pr", "--worktree"),
        ] {
            let args = argv(&[a, "x", b, "y"]);
            let outcome = cat.parse_raw_args_with_profile(path, &args, FrontendKind::Cli);
            assert!(outcome.is_err(), "{path:?} {a} + {b} must conflict");
        }
    }
}

/// `--yolo` and `--auto` must not select, imply or default a session target.
#[test]
fn yolo_and_auto_stay_inert_for_session_worktree_selectors() {
    let cat = CommandCatalogue::get();
    for path in [&["chat"][..], &["exec", "prompt"][..]] {
        let command = cat.lookup(path).unwrap();
        for mode in ["yolo", "auto"] {
            let flag = command.find_flag(mode).unwrap();
            for selector in ["branch", "pr", "worktree"] {
                assert!(
                    !flag.implies.contains(&selector),
                    "--{mode} must not imply --{selector} on {path:?}"
                );
            }
        }
        for mode in ["--yolo", "--auto"] {
            let parsed = cat
                .parse_raw_args_with_profile(path, &argv(&[mode]), FrontendKind::Cli)
                .unwrap();
            for selector in ["branch", "pr", "worktree"] {
                assert_eq!(
                    parsed.flag_string(selector),
                    None,
                    "{mode} set --{selector}"
                );
            }
        }
    }
    // Only the workflow command keeps the boolean `--worktree` implication.
    let workflow = cat.lookup(&["exec", "workflow"]).unwrap();
    assert!(workflow
        .find_flag("yolo")
        .unwrap()
        .implies
        .contains(&"worktree"));
}

#[test]
fn exec_workflow_worktree_is_still_boolean_and_has_no_branch_or_pr() {
    let cat = CommandCatalogue::get();
    let workflow = cat.lookup(&["exec", "workflow"]).unwrap();
    assert!(matches!(
        workflow.find_flag("worktree").unwrap().kind,
        FlagKind::Bool
    ));
    assert!(workflow.find_flag("branch").is_none());
    assert!(workflow.find_flag("pr").is_none());
    let parsed = cat
        .parse_raw_args_with_profile(
            &["exec", "workflow"],
            &argv(&["--worktree", "wf.toml"]),
            FrontendKind::Cli,
        )
        .unwrap();
    assert_eq!(parsed.flag_bool("worktree"), Some(true));
    assert_eq!(parsed.argument("workflow").as_deref(), Some("wf.toml"));
}

#[test]
fn remote_exec_prompt_has_none_of_the_session_worktree_flags() {
    let cat = CommandCatalogue::get();
    let remote = cat.lookup(&["remote", "exec", "prompt"]).unwrap();
    for name in ["branch", "pr", "worktree"] {
        assert!(remote.find_flag(name).is_none(), "--{name}");
    }
    for selector in ["--branch", "--pr", "--worktree"] {
        assert!(cat
            .parse_raw_args_with_profile(
                &["remote", "exec", "prompt"],
                &argv(&[selector, "x", "p"]),
                FrontendKind::Cli
            )
            .is_err());
    }
}

#[test]
fn selectors_before_the_prompt_are_parsed_and_after_it_are_refused() {
    use crate::command::commands::exec_prompt::refuse_selector_in_prompt;
    let cat = CommandCatalogue::get();
    for frontend in [FrontendKind::Cli, FrontendKind::Tui] {
        // The documented order selects the worktree.
        let parsed = cat
            .parse_raw_args_with_profile(
                &["exec", "prompt"],
                &argv(&["--pr", "482", "Fix the failing test"]),
                frontend,
            )
            .unwrap();
        assert_eq!(parsed.flag_string("pr").as_deref(), Some("482"));
        let tokens = parsed.arguments("prompt");
        assert!(refuse_selector_in_prompt(&tokens, &["exec", "prompt"]).is_ok());

        // After the prompt the greedy argument swallows the selector; the
        // input adapter must refuse it instead of launching in place.
        for selector in ["--branch", "--pr", "--worktree"] {
            let inline = format!("{selector}=x");
            for spelling in [vec![selector, "x"], vec![inline.as_str()]] {
                let mut args = vec!["Fix the failing test"];
                args.extend(spelling);
                let parsed = cat
                    .parse_raw_args_with_profile(&["exec", "prompt"], &argv(&args), frontend)
                    .unwrap();
                assert_eq!(parsed.flag_string(&selector[2..]), None);
                let error =
                    refuse_selector_in_prompt(&parsed.arguments("prompt"), &["exec", "prompt"])
                        .unwrap_err()
                        .to_string();
                assert!(error.contains("after the prompt"), "{args:?}: {error}");
            }
        }
    }
    // Prompt text that merely mentions a selector, quoted as one argument,
    // stays a prompt.
    assert!(refuse_selector_in_prompt(
        &["explain what --pr does".to_string()],
        &["exec", "prompt"]
    )
    .is_ok());
    assert!(refuse_selector_in_prompt(&["--yolo".to_string()], &["exec", "prompt"]).is_ok());
}

/// WI 0139: settings are config keys, set per invocation with `--set`. The
/// per-setting flags are gone and are plain unknown flags (no removed-flag
/// hint), on every frontend's parser.
#[test]
fn removed_per_setting_flags_are_unknown_flags() {
    let cat = CommandCatalogue::get();
    let cases: &[(&[&str], &str)] = &[
        (&["chat"], "launch-mode"),
        (&["exec", "prompt"], "launch-mode"),
        (&["exec", "workflow"], "launch-mode"),
        (&["exec", "workflow"], "max-concurrent"),
        (&["remote", "exec", "workflow"], "max-concurrent"),
        (&["chat"], "non-interactive"),
        (&["exec", "prompt"], "non-interactive"),
        (&["exec", "workflow"], "non-interactive"),
        (&["ready"], "non-interactive"),
        (&["daemon", "start"], "port"),
        (&["daemon", "start"], "workdirs"),
        (&["daemon", "start"], "refresh-key"),
        (&["daemon", "start"], "dangerously-skip-auth"),
    ];
    for (path, flag) in cases {
        let spec = cat.lookup(path).unwrap();
        assert!(
            spec.find_flag(flag).is_none(),
            "{path:?} still declares --{flag}"
        );
        let err = cat
            .parse_raw_args(path, &[format!("--{flag}"), "1".into()])
            .expect_err("a removed flag must not parse");
        assert!(
            matches!(err, crate::command::error::CommandError::UnknownFlag { .. }),
            "{path:?} --{flag}: {err:?}"
        );
        assert!(
            cat.removed_flag_hint([format!("--{flag}")]).is_none(),
            "--{flag} must fail as an unknown flag, not with a migration hint"
        );
    }
}

/// `--headless` (short `-n`) replaces `--non-interactive` on every command
/// that had it.
#[test]
fn headless_replaces_non_interactive_with_the_same_short_flag() {
    let cat = CommandCatalogue::get();
    for path in [
        &["chat"][..],
        &["exec", "prompt"],
        &["exec", "workflow"],
        &["remote", "exec", "prompt"],
        &["remote", "exec", "workflow"],
        &["ready"],
        &["specs", "amend"],
        &["squad"],
    ] {
        let spec = cat.lookup(path).unwrap();
        let flag = spec
            .find_flag("headless")
            .unwrap_or_else(|| panic!("{path:?} must declare --headless"));
        assert_eq!(flag.short, Some('n'), "{path:?}");
    }
}

/// `config migrate` and `config prerequisite-grants` are gone; `unset` is new.
#[test]
fn config_subcommands_are_show_get_set_unset() {
    let cat = CommandCatalogue::get();
    let config = cat.lookup(&["config"]).unwrap();
    let names: Vec<&str> = config.subcommands.iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["show", "get", "set", "unset"]);
    for leaf in ["set", "unset"] {
        let spec = cat.lookup(&["config", leaf]).unwrap();
        for flag in ["global", "repo", "task"] {
            assert!(spec.find_flag(flag).is_some(), "config {leaf} --{flag}");
        }
    }
}

/// `--set` is the one global flag: declared once, visible to every frontend.
#[test]
fn set_is_a_global_repeatable_flag() {
    let cat = CommandCatalogue::get();
    let set = cat
        .global_flags()
        .iter()
        .find(|f| f.long == "set")
        .expect("--set must be global");
    assert!(matches!(set.kind, FlagKind::VecString));
    assert!(matches!(set.frontends, FrontendVisibility::All));
    // A global flag parses on any command, in any position before `--`.
    let parsed = cat
        .parse_raw_args(
            &["agents", "status"],
            &["--set".into(), "a=1".into(), "--set=b=2".into()],
        )
        .unwrap();
    assert_eq!(parsed.flag_strings("set"), vec!["a=1", "b=2"]);
}
