//! WI-0097 — clap ↔ raw-args projection parity.
//!
//! The API frontend parses a pre-tokenized argv with
//! [`CommandCatalogue::parse_raw_args`] (the raw-args projection), while the CLI
//! frontend parses the same catalogue through clap
//! ([`CommandCatalogue::build_clap_command`]). Both projections derive
//! everything from the single-source-of-truth [`CommandSpec`] data, so for any
//! given command + argv they MUST produce identical typed results.
//!
//! The headline test [`clap_and_raw_args_agree_for_every_command`] iterates
//! **every command in the catalogue**, feeds a representative argv through both
//! projections, and asserts the typed values match. It is catalogue-driven, so a
//! new command added to the catalogue is covered automatically — the projections
//! cannot silently drift.
//!
//! The remaining tests pin the tricky parser edge cases called out in the work
//! item (`--flag=value` vs `--flag value`, repeated flags, hyphen-looking values
//! after `--`, negative numbers, and the greedy trailing positional of
//! `exec prompt`) and the per-frontend flag-default policy (Finding D).

use std::path::PathBuf;

use clap::ArgMatches;

use crate::command::dispatch::catalogue::{
    ArgumentKind, ArgumentSpec, CommandCatalogue, CommandSpec, FlagKind, FlagSpec, FrontendKind,
    FrontendVisibility,
};
use crate::command::dispatch::projections::raw_args::ParsedArgs;

// ─── A comparable, projection-agnostic typed value ───────────────────────────

/// One flag/argument value normalized so the clap and raw-args projections can
/// be compared regardless of how each stores it internally.
#[derive(Debug, PartialEq)]
enum Cmp {
    Bool(bool),
    Str(Option<String>),
    Strs(Vec<String>),
    Path(Option<PathBuf>),
    U16(Option<u16>),
    Usize(Option<usize>),
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn to_vec(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Mirror of the clap projection's visibility filter: only CLI-visible flags are
/// present in the clap `Command`, so only those can be exercised through both
/// projections in a single argv.
fn flag_visible_to_cli(f: &FlagSpec) -> bool {
    matches!(
        f.frontends,
        FrontendVisibility::All | FrontendVisibility::CliOnly | FrontendVisibility::CliAndTui
    )
}

/// Recursively collect every leaf command (one with no subcommands) plus its
/// full path from the catalogue root.
fn collect_leaves(
    spec: &'static CommandSpec,
    path: Vec<&'static str>,
    out: &mut Vec<(Vec<&'static str>, &'static CommandSpec)>,
) {
    if spec.subcommands.is_empty() {
        if !path.is_empty() {
            out.push((path, spec));
        }
        return;
    }
    for sub in spec.subcommands {
        let mut p = path.clone();
        p.push(sub.name);
        collect_leaves(sub, p, out);
    }
}

/// Build a representative argv for a command that exercises as many of its
/// CLI-visible flags as can coexist (skipping any that conflict with an
/// already-included flag) followed by one value per positional argument.
///
/// Returns the argv (tokens after the command path) and the flags actually
/// included, so the caller compares only what was exercised.
fn build_argv(spec: &CommandSpec) -> (Vec<String>, Vec<&'static FlagSpec>) {
    let mut argv: Vec<String> = Vec::new();
    let mut included: Vec<&'static FlagSpec> = Vec::new();

    for f in spec.flags {
        if !flag_visible_to_cli(f) {
            continue;
        }
        // Skip flags that clap would reject as mutually exclusive with a flag
        // already in the argv (conflicts are symmetric in intent).
        let conflicts = included
            .iter()
            .any(|g| g.conflicts_with.contains(&f.long) || f.conflicts_with.contains(&g.long));
        if conflicts {
            continue;
        }

        match f.kind {
            FlagKind::Bool => argv.push(format!("--{}", f.long)),
            FlagKind::String | FlagKind::OptionalString => {
                argv.push(format!("--{}", f.long));
                argv.push(format!("val_{}", f.long));
            }
            FlagKind::Enum(allowed) => {
                argv.push(format!("--{}", f.long));
                argv.push(allowed[0].to_string());
            }
            FlagKind::VecString => {
                argv.push(format!("--{}", f.long));
                argv.push("a".to_string());
                argv.push(format!("--{}", f.long));
                argv.push("b".to_string());
            }
            FlagKind::Path | FlagKind::OptionalPath => {
                argv.push(format!("--{}", f.long));
                argv.push(format!("path_{}", f.long));
            }
            FlagKind::U16 => {
                argv.push(format!("--{}", f.long));
                argv.push("123".to_string());
            }
            FlagKind::UsizeAtLeastOne => {
                argv.push(format!("--{}", f.long));
                argv.push("3".to_string());
            }
        }
        included.push(f);
    }

    // Positionals last so a greedy trailing var-arg captures exactly its tokens.
    for a in spec.arguments {
        match a.kind {
            ArgumentKind::TrailingVarArgs => {
                argv.push("tv1".to_string());
                argv.push("tv2".to_string());
            }
            _ => argv.push(format!("arg_{}", a.name)),
        }
    }

    (argv, included)
}

/// Navigate a root `ArgMatches` down `path` to the leaf command's matches.
fn descend<'a>(m: &'a ArgMatches, path: &[&str]) -> &'a ArgMatches {
    let mut cur = m;
    for seg in path {
        cur = cur
            .subcommand_matches(seg)
            .unwrap_or_else(|| panic!("missing clap subcommand '{seg}' in path {path:?}"));
    }
    cur
}

/// Full clap parse of `path` + `argv`, returning the (owned) leaf matches.
fn clap_leaf(cat: &CommandCatalogue, path: &[&str], argv: &[&str]) -> ArgMatches {
    let mut full: Vec<String> = vec!["awman".to_string()];
    full.extend(path.iter().map(|s| s.to_string()));
    full.extend(argv.iter().map(|s| s.to_string()));
    let m = cat
        .build_clap_command()
        .try_get_matches_from(&full)
        .unwrap_or_else(|e| panic!("clap parse failed for {path:?} argv {argv:?}: {e}"));
    descend(&m, path).clone()
}

fn clap_flag(m: &ArgMatches, f: &FlagSpec) -> Cmp {
    match f.kind {
        FlagKind::Bool => Cmp::Bool(m.get_flag(f.long)),
        FlagKind::String | FlagKind::OptionalString | FlagKind::Enum(_) => {
            Cmp::Str(m.get_one::<String>(f.long).cloned())
        }
        FlagKind::VecString => Cmp::Strs(
            m.get_many::<String>(f.long)
                .map(|v| v.cloned().collect())
                .unwrap_or_default(),
        ),
        FlagKind::Path | FlagKind::OptionalPath => {
            Cmp::Path(m.get_one::<String>(f.long).map(PathBuf::from))
        }
        FlagKind::U16 => Cmp::U16(m.get_one::<u16>(f.long).copied()),
        FlagKind::UsizeAtLeastOne => Cmp::Usize(m.get_one::<usize>(f.long).copied()),
    }
}

fn raw_flag(p: &ParsedArgs, f: &FlagSpec) -> Cmp {
    match f.kind {
        FlagKind::Bool => Cmp::Bool(p.flag_bool(f.long) == Some(true)),
        FlagKind::String | FlagKind::OptionalString => Cmp::Str(p.flag_string(f.long)),
        FlagKind::Enum(_) => Cmp::Str(p.flag_enum(f.long)),
        FlagKind::VecString => Cmp::Strs(p.flag_strings(f.long)),
        FlagKind::Path | FlagKind::OptionalPath => Cmp::Path(p.flag_path(f.long)),
        FlagKind::U16 => Cmp::U16(p.flag_u16(f.long)),
        FlagKind::UsizeAtLeastOne => Cmp::Usize(p.flag_usize(f.long)),
    }
}

fn arg_values(m: &ArgMatches, p: &ParsedArgs, a: &ArgumentSpec) -> (Cmp, Cmp) {
    match a.kind {
        ArgumentKind::TrailingVarArgs => {
            let c = m
                .get_many::<String>(a.name)
                .map(|v| v.cloned().collect())
                .unwrap_or_default();
            (Cmp::Strs(c), Cmp::Strs(p.arguments(a.name)))
        }
        _ => {
            let c = m.get_one::<String>(a.name).cloned();
            (Cmp::Str(c), Cmp::Str(p.argument(a.name)))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Headline catalogue-iterating parity test
// ═══════════════════════════════════════════════════════════════════════════

/// For every command in the catalogue, a representative argv parsed by clap
/// (CLI projection) and by `parse_raw_args` (API projection) yields identical
/// typed values for every exercised flag and argument. New commands are covered
/// automatically because the catalogue drives the iteration.
#[test]
fn clap_and_raw_args_agree_for_every_command() {
    let cat = CommandCatalogue::get();
    let mut leaves = Vec::new();
    collect_leaves(cat.root(), Vec::new(), &mut leaves);

    // Sanity: the walk must actually find the known leaf commands, otherwise a
    // silently-empty catalogue would make this test vacuously pass.
    assert!(
        leaves.len() >= 15,
        "expected the catalogue walk to find many leaf commands, found {}",
        leaves.len()
    );
    assert!(
        leaves.iter().any(|(p, _)| p == &["exec", "prompt"]),
        "exec prompt must be among the walked leaves"
    );

    for (path, spec) in &leaves {
        let (argv, flags) = build_argv(spec);
        let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

        let leaf_m = clap_leaf(cat, path, &argv_refs);
        let parsed = cat
            .parse_raw_args(path, &argv)
            .unwrap_or_else(|e| panic!("parse_raw_args failed for {path:?} argv {argv:?}: {e:?}"));

        for f in &flags {
            let c = clap_flag(&leaf_m, f);
            let r = raw_flag(&parsed, f);
            assert_eq!(
                c, r,
                "flag '{}' at {:?} diverged: clap={:?} raw={:?} (argv {:?})",
                f.long, path, c, r, argv
            );
        }

        for a in spec.arguments {
            let (c, r) = arg_values(&leaf_m, &parsed, a);
            assert_eq!(
                c, r,
                "argument '{}' at {:?} diverged: clap={:?} raw={:?} (argv {:?})",
                a.name, path, c, r, argv
            );

            // A path-typed positional must also be reachable via flag_path
            // with the same value clap parsed. Dispatch itself now reads
            // positionals only through `ResolvedArgs` (WI 0113 F-10), but the
            // two readings must not disagree: the CLI's clap matches expose a
            // positional under both accessors, and the raw-args projection has
            // to match that.
            if matches!(a.kind, ArgumentKind::Path | ArgumentKind::OptionalPath) {
                if let Some(s) = leaf_m.get_one::<String>(a.name) {
                    assert_eq!(
                        parsed.flag_path(a.name),
                        Some(PathBuf::from(s)),
                        "path positional '{}' at {:?} not reachable via flag_path",
                        a.name,
                        path
                    );
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Parser edge cases (work item: must be covered by the parity suite)
// ═══════════════════════════════════════════════════════════════════════════

/// `--flag=value` and `--flag value` are equivalent, and both agree with clap.
#[test]
fn edge_flag_equals_and_space_forms_parity() {
    let cat = CommandCatalogue::get();
    let path = ["exec", "prompt"];

    let spaced = cat
        .parse_raw_args(&path, &to_vec(&["--agent", "claude"]))
        .unwrap();
    let equals = cat
        .parse_raw_args(&path, &to_vec(&["--agent=claude"]))
        .unwrap();
    assert_eq!(spaced.flag_string("agent").as_deref(), Some("claude"));
    assert_eq!(equals.flag_string("agent").as_deref(), Some("claude"));

    let clap_spaced = clap_leaf(cat, &path, &["--agent", "claude"]);
    let clap_equals = clap_leaf(cat, &path, &["--agent=claude"]);
    assert_eq!(
        clap_spaced.get_one::<String>("agent").map(String::as_str),
        Some("claude")
    );
    assert_eq!(
        clap_equals.get_one::<String>("agent").map(String::as_str),
        Some("claude")
    );
}

/// A repeated `VecString` flag accumulates in order, identically to clap.
#[test]
fn edge_repeated_flag_accumulates_parity() {
    let cat = CommandCatalogue::get();
    let path = ["exec", "prompt"];
    let argv = ["--overlay", "/a", "--overlay", "/b"];

    let parsed = cat.parse_raw_args(&path, &to_vec(&argv)).unwrap();
    assert_eq!(parsed.flag_strings("overlay"), vec!["/a", "/b"]);

    let m = clap_leaf(cat, &path, &argv);
    let clap_vals: Vec<String> = m
        .get_many::<String>("overlay")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    assert_eq!(clap_vals, vec!["/a".to_string(), "/b".to_string()]);
}

/// Tokens that look like flags are captured as positionals verbatim after `--`,
/// identically to clap's trailing-var-arg + allow-hyphen-values handling.
#[test]
fn edge_flag_looking_values_after_double_dash_parity() {
    let cat = CommandCatalogue::get();
    let path = ["exec", "prompt"];
    let argv = ["--", "--foo", "-x"];

    let parsed = cat.parse_raw_args(&path, &to_vec(&argv)).unwrap();
    assert_eq!(parsed.arguments("prompt"), vec!["--foo", "-x"]);

    let m = clap_leaf(cat, &path, &argv);
    let clap_prompt: Vec<String> = m
        .get_many::<String>("prompt")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    assert_eq!(clap_prompt, vec!["--foo".to_string(), "-x".to_string()]);
}

/// Negative numbers are captured as trailing positionals (not misread as flags),
/// identically to clap.
#[test]
fn edge_negative_numbers_as_values_parity() {
    let cat = CommandCatalogue::get();
    let path = ["exec", "prompt"];
    let argv = ["hello", "-5", "-10"];

    let parsed = cat.parse_raw_args(&path, &to_vec(&argv)).unwrap();
    assert_eq!(parsed.arguments("prompt"), vec!["hello", "-5", "-10"]);

    let m = clap_leaf(cat, &path, &argv);
    let clap_prompt: Vec<String> = m
        .get_many::<String>("prompt")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    assert_eq!(
        clap_prompt,
        vec!["hello".to_string(), "-5".to_string(), "-10".to_string()]
    );
}

/// The greedy trailing positional of `exec prompt` joins all tokens with single
/// spaces for `argument()` and preserves them for `arguments()`, agreeing with
/// clap's collected values.
#[test]
fn edge_greedy_trailing_positional_parity() {
    let cat = CommandCatalogue::get();
    let path = ["exec", "prompt"];
    let argv = ["fix", "the", "bug"];

    let parsed = cat.parse_raw_args(&path, &to_vec(&argv)).unwrap();
    assert_eq!(parsed.argument("prompt").as_deref(), Some("fix the bug"));
    assert_eq!(parsed.arguments("prompt"), vec!["fix", "the", "bug"]);

    let m = clap_leaf(cat, &path, &argv);
    let clap_prompt: Vec<String> = m
        .get_many::<String>("prompt")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    assert_eq!(clap_prompt, vec!["fix", "the", "bug"]);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Type-coercion errors: raw_args rejects exactly where clap rejects
// ═══════════════════════════════════════════════════════════════════════════

/// A non-numeric u16, a bad enum value, and an unknown flag each produce the
/// expected typed [`CommandError`] from `parse_raw_args`, and clap rejects the
/// same inputs.
///
/// The unknown-flag case uses `config get` (a plain-positional command) rather
/// than `exec prompt`: `exec prompt`'s trailing var-arg is declared
/// `allow_hyphen_values`, so clap ABSORBS a leading `--unknown` into the prompt
/// positional instead of rejecting it. `parse_raw_args` deliberately stays
/// stricter there (unknown flag → structured error, never a silently-dropped
/// flag — WI-0097 Finding A), so the two projections intentionally diverge for
/// that one shape and it must not be asserted as parity.
#[test]
fn coercion_errors_produce_typed_errors_and_clap_rejects() {
    use crate::command::error::CommandError;
    let cat = CommandCatalogue::get();

    // Non-numeric value for a u16 flag → InvalidFlagValue; clap also rejects.
    let err = cat
        .parse_raw_args(&["api", "start"], &to_vec(&["--port", "not-a-number"]))
        .unwrap_err();
    assert!(
        matches!(err, CommandError::InvalidFlagValue { .. }),
        "got {err:?}"
    );
    assert!(cat
        .build_clap_command()
        .try_get_matches_from(["awman", "api", "start", "--port", "not-a-number"])
        .is_err());

    // Bad enum value → InvalidFlagValue; clap also rejects.
    let err = cat
        .parse_raw_args(
            &["remote", "session", "start"],
            &to_vec(&["--type", "banana"]),
        )
        .unwrap_err();
    assert!(
        matches!(err, CommandError::InvalidFlagValue { .. }),
        "got {err:?}"
    );
    assert!(cat
        .build_clap_command()
        .try_get_matches_from(["awman", "remote", "session", "start", "--type", "banana"])
        .is_err());

    // Unknown flag → UnknownFlag; clap also rejects (no trailing var-arg here).
    let err = cat
        .parse_raw_args(&["config", "get"], &to_vec(&["--definitely-not-a-flag"]))
        .unwrap_err();
    assert!(
        matches!(err, CommandError::UnknownFlag { .. }),
        "got {err:?}"
    );
    assert!(cat
        .build_clap_command()
        .try_get_matches_from(["awman", "config", "get", "--definitely-not-a-flag"])
        .is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
//  Per-frontend flag defaults (Finding D)
// ═══════════════════════════════════════════════════════════════════════════

/// The API profile resolves `non-interactive=true` (forced) and `yolo=true`
/// (overridable default, per architecture-decisions.md D1) via the catalogue,
/// while the CLI dispatch of the very same command + argv is unaffected.
#[test]
fn api_profile_applies_defaults_cli_dispatch_unaffected() {
    let cat = CommandCatalogue::get();
    let path = ["exec", "prompt"];
    let argv = to_vec(&["hello", "world"]);

    let api = cat
        .parse_raw_args_with_profile(&path, &argv, FrontendKind::Api)
        .unwrap();
    assert_eq!(
        api.flag_bool("non-interactive"),
        Some(true),
        "API profile must force non-interactive=true"
    );
    assert_eq!(
        api.flag_bool("yolo"),
        Some(true),
        "API profile yolo default is true (overridable) — see architecture-decisions.md D1"
    );

    let cli = cat
        .parse_raw_args_with_profile(&path, &argv, FrontendKind::Cli)
        .unwrap();
    assert_eq!(
        cli.flag_bool("non-interactive"),
        None,
        "CLI dispatch must NOT have profile-forced non-interactive"
    );
    assert_eq!(
        cli.flag_bool("yolo"),
        None,
        "CLI dispatch must NOT have profile-defaulted yolo"
    );

    // The command payload (positional prompt) resolves identically either way.
    assert_eq!(api.argument("prompt"), cli.argument("prompt"));
    assert_eq!(cli.argument("prompt").as_deref(), Some("hello world"));
}

/// The yolo default is overridable: an explicit request value survives under the
/// API profile (whereas non-interactive is forced regardless).
#[test]
fn api_profile_yolo_is_overridable_non_interactive_is_forced() {
    let cat = CommandCatalogue::get();
    let path = ["exec", "prompt"];

    // Explicitly opting into --plan (mutually exclusive with yolo in the
    // catalogue) genuinely overrides the overridable yolo default: it is
    // suppressed because a conflicting flag is present, so yolo stays unset.
    // non-interactive stays forced regardless.
    let with_plan = cat
        .parse_raw_args_with_profile(&path, &to_vec(&["--plan", "hi"]), FrontendKind::Api)
        .unwrap();
    assert_eq!(with_plan.flag_bool("plan"), Some(true));
    assert_eq!(with_plan.flag_bool("non-interactive"), Some(true));
    assert_eq!(with_plan.flag_bool("yolo"), None);
}

// ─── WI 0113 F-10: the catalogue owns defaults and implications ─────────────
//
// The two regression guards for the root cause F-10 names: six literal
// defaults restated in `Dispatch::build_command` beside the catalogue's own
// `FlagDefault`, and three hand-written `if`s restating `implies`, which had
// no non-test consumer at all. Both sweeps are catalogue-driven, so a flag
// added tomorrow is covered without touching this file.

use std::sync::Arc;

use crate::command::commands::squad::commands::SquadSubcommand;
use crate::command::dispatch::catalogue::FlagDefault;
use crate::command::dispatch::tests::FakeCommandFrontend;
use crate::command::dispatch::{BuiltCommand, Dispatch, Engines, ResolvedFlags};
use crate::command::error::CommandError;

/// Every leaf command the catalogue can build, with its canonical path.
fn buildable_leaves() -> Vec<(Vec<&'static str>, &'static CommandSpec)> {
    let mut leaves = Vec::new();
    collect_leaves(CommandCatalogue::get().root(), Vec::new(), &mut leaves);
    leaves
}

/// A frontend that supplies nothing at all, so every value must come from the
/// catalogue.
fn empty_frontend() -> FakeCommandFrontend {
    FakeCommandFrontend::new()
}

/// A frontend carrying a placeholder for every positional argument and every
/// required flag, so `build_command` gets far enough to construct the command
/// whatever leaf is under test.
fn frontend_with_required_input(spec: &'static CommandSpec) -> FakeCommandFrontend {
    let mut frontend = FakeCommandFrontend::new();
    for argument in spec.arguments {
        frontend
            .args
            .insert(argument.name.to_string(), "placeholder".to_string());
    }
    for flag in spec.flags {
        if flag.optional {
            continue;
        }
        match flag.kind {
            FlagKind::Bool => {
                frontend.bools.insert(flag.long.to_string(), true);
            }
            FlagKind::String | FlagKind::OptionalString => {
                frontend
                    .strings
                    .insert(flag.long.to_string(), "placeholder".to_string());
            }
            FlagKind::Enum(values) => {
                frontend
                    .enums
                    .insert(flag.long.to_string(), values[0].to_string());
            }
            FlagKind::VecString => {
                frontend
                    .strings_vec
                    .insert(flag.long.to_string(), vec!["placeholder".to_string()]);
            }
            FlagKind::Path | FlagKind::OptionalPath => {
                frontend
                    .paths
                    .insert(flag.long.to_string(), PathBuf::from("placeholder"));
            }
            FlagKind::U16 => {
                frontend.u16s.insert(flag.long.to_string(), 1);
            }
            FlagKind::UsizeAtLeastOne => {
                frontend.usizes.insert(flag.long.to_string(), 1);
            }
        }
    }
    frontend
}

fn dispatch_for(frontend: FakeCommandFrontend) -> Dispatch<FakeCommandFrontend> {
    let tmp = tempfile::tempdir().expect("temp root");
    let session = crate::data::session::Session::for_tests(tmp.path());
    Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session)),
        Engines::for_tests(tmp.path()),
    )
}

fn resolve_with(frontend: FakeCommandFrontend, path: &[&str]) -> ResolvedFlags {
    dispatch_for(frontend)
        .resolve_flags(path)
        .unwrap_or_else(|e| panic!("resolve {path:?}: {e}"))
}

/// Sweep 1 — for every flag in the catalogue that declares a `FlagDefault`,
/// resolving with nothing supplied yields exactly that default. This is what
/// the six literals at the old `build_command:406, :557, :691, :716, :944,
/// :1008` were silently allowed to disagree with.
#[test]
fn every_flag_default_is_what_resolution_answers() {
    for (path, spec) in buildable_leaves() {
        let flags = resolve_with(empty_frontend(), &path);
        for flag in spec.flags {
            let where_ = format!("{} --{}", path.join(" "), flag.long);
            match flag.default {
                FlagDefault::None => match flag.kind {
                    FlagKind::Bool => {
                        assert!(!flags.bool(flag.long), "{where_} must default false")
                    }
                    FlagKind::VecString => {
                        assert!(
                            flags.strs(flag.long).is_empty(),
                            "{where_} must default empty"
                        )
                    }
                    _ => {}
                },
                FlagDefault::Bool(expected) => {
                    assert_eq!(flags.bool(flag.long), expected, "{where_}")
                }
                FlagDefault::Str(expected) => {
                    assert_eq!(flags.str(flag.long), Some(expected), "{where_}")
                }
                FlagDefault::U16(expected) => {
                    assert_eq!(flags.u16(flag.long), Some(expected), "{where_}")
                }
                FlagDefault::EmptyVec => {
                    assert!(flags.strs(flag.long).is_empty(), "{where_}")
                }
            }
        }
    }
}

/// Sweep 2 — every `implies` edge in the catalogue is honoured. Before F-10
/// this relation had no non-test consumer: dispatch re-derived
/// `json → non-interactive` and `yolo|auto → worktree` by hand, in three
/// places, for three of the commands that declare them.
#[test]
fn every_implies_edge_is_honoured() {
    let mut edges_seen = 0;
    for (path, spec) in buildable_leaves() {
        for flag in spec.flags {
            if flag.implies.is_empty() {
                continue;
            }
            assert!(
                matches!(flag.kind, FlagKind::Bool),
                "{} --{}: only boolean flags may imply another flag",
                path.join(" "),
                flag.long
            );
            let mut frontend = frontend_with_required_input(spec);
            frontend.bools.insert(flag.long.to_string(), true);
            // A flag this one conflicts with must not also be supplied, or
            // resolution refuses before the implication runs.
            for conflict in flag.conflicts_with {
                frontend.bools.remove(*conflict);
                frontend.strings.remove(*conflict);
            }
            let flags = resolve_with(frontend, &path);
            for target in flag.implies {
                // The shared flag arrays give `--yolo` to commands with no
                // `--worktree` to set; there the edge is inert by design.
                if spec.find_flag(target).is_none() {
                    continue;
                }
                edges_seen += 1;
                assert!(
                    flags.bool(target),
                    "{} --{} must imply --{}",
                    path.join(" "),
                    flag.long,
                    target
                );
            }
        }
    }
    assert!(
        edges_seen >= 3,
        "the catalogue's json/yolo/auto implications must be exercised, saw {edges_seen}"
    );
}

/// Sweep 3 — the non-trivial defaults do not merely resolve correctly, they
/// reach the constructed command. This is the half that the deleted literals
/// used to satisfy by restating them.
///
/// The list is checked for completeness against the catalogue below, so a new
/// non-trivial default cannot be added without an assertion here.
#[test]
fn every_non_trivial_default_reaches_the_built_command() {
    let mut covered: Vec<(Vec<&str>, &str)> = Vec::new();
    let build = |path: &[&'static str]| -> BuiltCommand {
        let spec = CommandCatalogue::get().lookup(path).expect("spec");
        dispatch_for(frontend_with_required_input(spec))
            .build_command(path)
            .unwrap_or_else(|e| panic!("build {path:?}: {e}"))
    };

    match build(&["init"]) {
        BuiltCommand::Init(cmd) => assert_eq!(cmd.flags().agent, "claude"),
        _ => panic!("expected Init"),
    }
    covered.push((vec!["init"], "agent"));

    match build(&["api", "start"]) {
        BuiltCommand::ApiServer(cmd) => match cmd.subcommand() {
            crate::command::commands::api_server::ApiServerSubcommand::Start(flags) => {
                assert_eq!(flags.port, 9876)
            }
            _ => panic!("expected api start"),
        },
        _ => panic!("expected ApiServer"),
    }
    covered.push((vec!["api", "start"], "port"));

    match build(&["squad", "start"]) {
        BuiltCommand::Squad(cmd) => match cmd.subcommand() {
            SquadSubcommand::Start(flags) => assert_eq!(flags.port, 0),
            _ => panic!("expected squad start"),
        },
        _ => panic!("expected Squad"),
    }
    covered.push((vec!["squad", "start"], "port"));

    match build(&["squad", "add"]) {
        BuiltCommand::Squad(cmd) => match cmd.subcommand() {
            SquadSubcommand::Add(request) => {
                let prefilled = request.prefilled.as_ref().expect("scripted create");
                assert_eq!(
                    prefilled.interval_secs,
                    6 * 3600,
                    "--interval defaults to 6h"
                );
                assert_eq!(
                    prefilled.mount_scope,
                    crate::data::fs::task_store::MountScope::GitRoot,
                    "--mount-scope defaults to gitroot"
                );
            }
            _ => panic!("expected squad add"),
        },
        _ => panic!("expected Squad"),
    }
    covered.push((vec!["squad", "add"], "interval"));
    covered.push((vec!["squad", "add"], "mount-scope"));

    match build(&["new", "workflow"]) {
        BuiltCommand::New(cmd) => match cmd.subcommand() {
            crate::command::commands::new::NewSubcommand::Workflow(flags) => {
                assert_eq!(flags.format, "toml")
            }
            _ => panic!("expected new workflow"),
        },
        _ => panic!("expected New"),
    }
    covered.push((vec!["new", "workflow"], "format"));

    match build(&["remote", "session", "start"]) {
        BuiltCommand::Remote(cmd) => match cmd.subcommand() {
            crate::command::commands::remote::RemoteSubcommand::SessionStart(flags) => {
                assert_eq!(flags.session_type, "local")
            }
            _ => panic!("expected remote session start"),
        },
        _ => panic!("expected Remote"),
    }
    covered.push((vec!["remote", "session", "start"], "type"));

    // Completeness: every non-trivial default in the catalogue is asserted
    // above. `Bool(false)` and `EmptyVec` are the trivial ones — they equal
    // the absent-value reading, so sweep 1 alone covers them.
    let mut expected: Vec<(Vec<&str>, &str)> = Vec::new();
    for (path, spec) in buildable_leaves() {
        for flag in spec.flags {
            if matches!(flag.default, FlagDefault::Str(_) | FlagDefault::U16(_))
                || matches!(flag.default, FlagDefault::Bool(true))
            {
                expected.push((path.clone(), flag.long));
            }
        }
    }
    expected.sort();
    covered.sort();
    assert_eq!(
        covered, expected,
        "a non-trivial FlagDefault was added or removed without updating this test"
    );
}

/// Sweep 4 — every command leaf in the catalogue actually builds through
/// `Dispatch`, and every grouping-only spec refuses.
///
/// This is the registration guard for `CommandSpec::build`: a leaf that
/// arrives without a Layer 2 command — the shape of F-01 — lands in
/// `NOT_RUNNABLE` or fails here. This is the regression guard that would have
/// caught F-01 when `squad attach` existed in the catalogue but not Dispatch.
#[test]
fn every_catalogue_command_leaf_is_buildable_by_dispatch() {
    /// Grouping parents. Every leaf command is buildable by Dispatch.
    const NOT_RUNNABLE: &[&[&str]] = &[
        &[],
        &["specs"],
        &["config"],
        &["exec"],
        &["api"],
        &["remote"],
        &["remote", "exec"],
        &["remote", "session"],
        &["new"],
    ];

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
    let mut specs = Vec::new();
    walk(CommandCatalogue::get().root(), Vec::new(), &mut specs);

    for (path, spec) in specs {
        let result = dispatch_for(frontend_with_required_input(spec)).build_command(&path);
        if NOT_RUNNABLE.contains(&path.as_slice()) {
            assert!(
                matches!(result, Err(CommandError::UnknownCommand { .. })),
                "{path:?} is not runnable and must refuse with UnknownCommand"
            );
        } else {
            assert!(
                result.is_ok(),
                "{path:?} must be buildable by Dispatch: {}",
                result.err().map(|e| e.to_string()).unwrap_or_default()
            );
        }
    }
}
