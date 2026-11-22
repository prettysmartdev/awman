//! `FlagKind::OptionalEnum` behaves the same in every projection (WI 0070):
//! a bare flag means its missing value, an explicit value needs `=`, the next
//! token is never consumed, identical repeats fold and conflicting repeats
//! are refused.

use crate::command::dispatch::build;
use crate::command::dispatch::catalogue::{
    ApiAvailability, ArgumentKind, ArgumentSpec, CommandCatalogue, CommandSpec, FlagDefault,
    FlagKind, FlagSpec, FrontendVisibility, GatewayNeed,
};
use crate::command::dispatch::projections::raw_args::parse_against_spec;
use crate::command::error::CommandError;
use crate::data::automation::AutomationMode;

/// A private spec with the real `--automation` kind and a greedy prompt, so
/// the syntax can be exercised without the real commands' pending refusal.
const PROBE: CommandSpec = CommandSpec {
    name: "probe",
    aliases: &[],
    help: "test probe",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "prompt",
        help: "prompt",
        kind: ArgumentKind::TrailingVarArgs,
        optional: true,
    }],
    flags: &[
        FlagSpec {
            long: "automation",
            short: None,
            help: "automation",
            kind: FlagKind::OptionalEnum {
                values: AutomationMode::VALUES,
                missing: AutomationMode::MISSING_VALUE,
            },
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "model",
            short: None,
            help: "model",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
    api_availability: ApiAvailability::Disabled,
    build: build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: true,
    relay_allowed: false,
};

const PATH: &[&str] = &["probe"];

fn args(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| (*token).to_string()).collect()
}

fn automation_flag() -> &'static FlagSpec {
    PROBE.find_flag("automation").unwrap()
}

fn invalid_automation(error: &CommandError) -> bool {
    matches!(error, CommandError::InvalidFlagValue { flag, .. } if flag == "automation")
}

#[test]
fn the_real_flag_uses_the_layer_0_enum_on_chat_and_exec_prompt_only() {
    let catalogue = CommandCatalogue::get();
    for path in [&["chat"][..], &["exec", "prompt"][..]] {
        let flag = catalogue
            .lookup(path)
            .and_then(|spec| spec.find_flag("automation"))
            .unwrap_or_else(|| panic!("{path:?}"));
        match flag.kind {
            FlagKind::OptionalEnum { values, missing } => {
                assert_eq!(values, AutomationMode::VALUES);
                assert_eq!(missing, AutomationMode::MISSING_VALUE);
                assert!(values.contains(&missing));
            }
            other => panic!("{path:?}: {other:?}"),
        }
        assert!(matches!(flag.default, FlagDefault::None));
        assert!(flag.short.is_none() && flag.implies.is_empty());
    }
    for path in [
        &["exec", "workflow"][..],
        &["remote", "exec", "prompt"][..],
        &["remote", "exec", "workflow"][..],
        &["ready"][..],
    ] {
        let spec = catalogue.lookup(path).unwrap();
        assert!(spec.find_flag("automation").is_none(), "{path:?}");
        assert!(spec.find_flag("agent-session").is_none(), "{path:?}");
    }
    // Owner-local selectors never reach a remote target through `args`.
    for path in [
        &["remote", "exec", "prompt"][..],
        &["remote", "exec", "workflow"][..],
    ] {
        let spec = catalogue.lookup(path).unwrap();
        assert!(spec.find_flag("settings-profile").is_none(), "{path:?}");
    }
    // No fork-style aliases are declared anywhere.
    for name in [
        "native-session",
        "native-print",
        "resume",
        "isolated-settings",
    ] {
        for path in [&["chat"][..], &["exec", "prompt"][..]] {
            assert!(catalogue.lookup(path).unwrap().find_flag(name).is_none());
        }
    }
}

#[test]
fn occurrences_fold_to_one_value() {
    let flag = automation_flag();
    let fold = |occurrences: &[Option<&str>]| {
        flag.resolve_optional_enum(PATH, occurrences.iter().copied())
    };
    assert_eq!(fold(&[]).unwrap(), None);
    assert_eq!(fold(&[None]).unwrap().as_deref(), Some("auto"));
    assert_eq!(fold(&[Some("auto")]).unwrap().as_deref(), Some("auto"));
    assert_eq!(
        fold(&[Some("require_full")]).unwrap().as_deref(),
        Some("require_full")
    );
    // Bare and explicit `auto` are the same request; identical repeats fold.
    assert_eq!(
        fold(&[None, Some("auto"), None]).unwrap().as_deref(),
        Some("auto")
    );
    assert_eq!(
        fold(&[Some("require_full"), Some("require_full")])
            .unwrap()
            .as_deref(),
        Some("require_full")
    );
    for conflicting in [
        &[None, Some("require_full")][..],
        &[Some("require_full"), Some("auto")][..],
    ] {
        let error = fold(conflicting).unwrap_err();
        assert!(invalid_automation(&error), "{error:?}");
        assert!(error.to_string().contains("conflicting"), "{error}");
        assert_eq!((error.exit_code(), error.http_status()), (2, 400));
    }
    for bad in ["", "full", "Auto", "true", "require-full"] {
        let error = fold(&[Some(bad)]).unwrap_err();
        assert!(invalid_automation(&error), "{bad:?}: {error:?}");
    }
}

#[test]
fn raw_args_never_consume_the_next_token_as_the_value() {
    let parse = |tokens: &[&str]| parse_against_spec(&PROBE, PATH, &args(tokens));

    let bare = parse(&["--automation"]).unwrap();
    assert_eq!(bare.flag_enum("automation").as_deref(), Some("auto"));

    // The token after a bare flag is the first prompt word, even when it
    // spells a valid value.
    let parsed = parse(&["--automation", "require_full", "is", "strict"]).unwrap();
    assert_eq!(parsed.flag_enum("automation").as_deref(), Some("auto"));
    assert_eq!(parsed.arguments("prompt"), ["require_full", "is", "strict"]);

    let explicit = parse(&["--automation=require_full", "review"]).unwrap();
    assert_eq!(
        explicit.flag_enum("automation").as_deref(),
        Some("require_full")
    );
    assert_eq!(explicit.arguments("prompt"), ["review"]);

    // A following flag is still parsed as a flag.
    let followed = parse(&["--automation", "--model", "m", "go"]).unwrap();
    assert_eq!(followed.flag_enum("automation").as_deref(), Some("auto"));
    assert_eq!(followed.flag_string("model").as_deref(), Some("m"));

    let omitted = parse(&["review"]).unwrap();
    assert!(!omitted.has_flag("automation"));
}

#[test]
fn raw_args_fold_repeats_and_refuse_bad_values() {
    let parse = |tokens: &[&str]| parse_against_spec(&PROBE, PATH, &args(tokens));

    let folded = parse(&["--automation", "--automation=auto"]).unwrap();
    assert_eq!(folded.flag_enum("automation").as_deref(), Some("auto"));

    for tokens in [
        &["--automation=auto", "--automation=require_full"][..],
        &["--automation=require_full", "--automation"][..],
        &["--automation="][..],
        &["--automation=full"][..],
    ] {
        let error = parse(tokens).unwrap_err();
        assert!(invalid_automation(&error), "{tokens:?}: {error:?}");
        assert_eq!(error.exit_code(), 2, "{tokens:?}");
    }
}

#[test]
fn raw_args_keep_the_flag_literal_after_the_delimiter_and_inside_the_prompt() {
    let parse = |tokens: &[&str]| parse_against_spec(&PROBE, PATH, &args(tokens));

    let literal = parse(&["--", "--automation=bogus"]).unwrap();
    assert!(!literal.has_flag("automation"));
    assert_eq!(literal.arguments("prompt"), ["--automation=bogus"]);

    let prose = parse(&["Explain --automation and --automation=require_full"]).unwrap();
    assert!(!prose.has_flag("automation"));
}

#[test]
fn forwarded_argv_round_trips_through_the_same_parser() {
    for tokens in [
        &["--automation", "go"][..],
        &["--automation=require_full", "go"][..],
    ] {
        let parsed = parse_against_spec(&PROBE, PATH, &args(tokens)).unwrap();
        let argv = parsed.to_argv(&PROBE);
        assert!(
            argv.iter().any(|token| token.starts_with("--automation=")),
            "{argv:?}"
        );
        let again = parse_against_spec(&PROBE, PATH, &argv).unwrap();
        assert_eq!(again, parsed, "{tokens:?}");
    }
}

/// The values clap collected for `--automation` on the deepest subcommand.
fn clap_automation(argv: &[&str]) -> Result<(Vec<String>, Vec<String>), clap::Error> {
    let matches = CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(argv)?;
    let mut current = &matches;
    while let Some((_, sub)) = current.subcommand() {
        current = sub;
    }
    let values = current
        .get_many::<String>("automation")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let prompt = current
        .try_get_many::<String>("prompt")
        .ok()
        .flatten()
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    Ok((values, prompt))
}

#[test]
fn clap_uses_the_same_syntax() {
    let (values, _) = clap_automation(&["awman", "chat", "--automation"]).unwrap();
    assert_eq!(values, ["auto"]);

    let (values, _) = clap_automation(&["awman", "chat", "--automation=require_full"]).unwrap();
    assert_eq!(values, ["require_full"]);

    let (values, _) = clap_automation(&["awman", "chat"]).unwrap();
    assert!(values.is_empty());

    // The next token is a prompt word, never the value.
    let (values, prompt) = clap_automation(&[
        "awman",
        "exec",
        "prompt",
        "--automation",
        "require_full",
        "is",
        "strict",
    ])
    .unwrap();
    assert_eq!(values, ["auto"]);
    assert_eq!(prompt, ["require_full", "is", "strict"]);

    // `chat` takes no positional, so a space-separated value is an error
    // rather than a silently consumed value.
    assert!(clap_automation(&["awman", "chat", "--automation", "require_full"]).is_err());
    assert!(clap_automation(&["awman", "chat", "--automation=full"]).is_err());
    assert!(clap_automation(&["awman", "chat", "--automation="]).is_err());

    // Repeats reach the catalogue's fold rule instead of a clap error.
    let (values, _) =
        clap_automation(&["awman", "chat", "--automation", "--automation=auto"]).unwrap();
    assert_eq!(values, ["auto", "auto"]);
    let (values, _) = clap_automation(&[
        "awman",
        "chat",
        "--automation=auto",
        "--automation=require_full",
    ])
    .unwrap();
    let flag = automation_flag();
    assert!(flag
        .resolve_optional_enum(&["chat"], values.iter().map(|v| Some(v.as_str())))
        .is_err());
}

#[test]
fn the_reference_and_web_catalogue_describe_the_optional_value() {
    let reference = CommandCatalogue::get().markdown_reference();
    assert!(reference.contains(
        "| `--automation` | enum, optional (`=value` only; bare = `auto`): auto, require_full |"
    ));
    let web = crate::command::daemon_api::web_projections::catalogue().to_string();
    assert!(web.contains("\"optional_enum\""));
    assert!(web.contains("\"missing\":\"auto\""));
}
