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

            // A path-typed positional must also be reachable via flag_path with
            // the same value clap parsed (Dispatch's flag_path-then-argument
            // fallback, used by `exec workflow`).
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

    let spaced = cat.parse_raw_args(&path, &to_vec(&["--agent", "claude"])).unwrap();
    let equals = cat.parse_raw_args(&path, &to_vec(&["--agent=claude"])).unwrap();
    assert_eq!(spaced.flag_string("agent").as_deref(), Some("claude"));
    assert_eq!(equals.flag_string("agent").as_deref(), Some("claude"));

    let clap_spaced = clap_leaf(cat, &path, &["--agent", "claude"]);
    let clap_equals = clap_leaf(cat, &path, &["--agent=claude"]);
    assert_eq!(clap_spaced.get_one::<String>("agent").map(String::as_str), Some("claude"));
    assert_eq!(clap_equals.get_one::<String>("agent").map(String::as_str), Some("claude"));
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
    assert!(matches!(err, CommandError::InvalidFlagValue { .. }), "got {err:?}");
    assert!(cat
        .build_clap_command()
        .try_get_matches_from(["awman", "api", "start", "--port", "not-a-number"])
        .is_err());

    // Bad enum value → InvalidFlagValue; clap also rejects.
    let err = cat
        .parse_raw_args(&["remote", "session", "start"], &to_vec(&["--type", "banana"]))
        .unwrap_err();
    assert!(matches!(err, CommandError::InvalidFlagValue { .. }), "got {err:?}");
    assert!(cat
        .build_clap_command()
        .try_get_matches_from(["awman", "remote", "session", "start", "--type", "banana"])
        .is_err());

    // Unknown flag → UnknownFlag; clap also rejects (no trailing var-arg here).
    let err = cat
        .parse_raw_args(&["config", "get"], &to_vec(&["--definitely-not-a-flag"]))
        .unwrap_err();
    assert!(matches!(err, CommandError::UnknownFlag { .. }), "got {err:?}");
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
