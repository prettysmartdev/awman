//! `edit` — edit existing work items and repository or global artifacts.

use super::*;

const EDIT_FLAGS: &[FlagSpec] = &[
    FlagSpec {
        long: "interview",
        short: None,
        help: "Ask clarifying questions before editing the existing item.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["headless", "json"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "amend",
        short: None,
        help: "Review and amend the existing item using available evidence.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "headless",
        short: Some('n'),
        help: "Run the edit agent without interactive input.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["interview"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "prompt",
        short: None,
        help: "Describe the changes to make.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "json",
        short: None,
        help: "Print the edit outcome as JSON and run headless.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["interview"],
        implies: &["headless"],
        optional: true,
    },
    FlagSpec {
        long: "allow-docker",
        short: None,
        help: "Allow the agent to use Docker when running.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "yolo",
        short: None,
        help: "Automatically approve agent actions.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "auto",
        short: None,
        help: "Run the agent in automatic mode.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "agent",
        short: None,
        help: "Select the agent to run.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "model",
        short: None,
        help: "Override the selected agent's model.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "overlay",
        short: None,
        help: "Add an overlay to the agent session.",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "agent-flag",
        short: None,
        help: "Pass a flag to the agent (replaces configured agent flags).",
        kind: FlagKind::VecString,
        default: FlagDefault::EmptyVec,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

const EDIT_ARTIFACT_FLAGS: &[FlagSpec] = &[
    EDIT_FLAGS[0],
    EDIT_FLAGS[1],
    EDIT_FLAGS[2],
    EDIT_FLAGS[3],
    FlagSpec {
        long: "global",
        short: None,
        help: "Select the global artifact instead of a repository artifact.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    EDIT_FLAGS[4],
    EDIT_FLAGS[5],
    EDIT_FLAGS[6],
    EDIT_FLAGS[7],
    EDIT_FLAGS[8],
    EDIT_FLAGS[9],
    EDIT_FLAGS[10],
    EDIT_FLAGS[11],
];

pub(super) const EDIT: CommandSpec = CommandSpec {
    name: "edit",
    aliases: &[],
    help: "Edit an existing spec, workflow, skill, or agent definition.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[&EDIT_SPEC, &EDIT_WORKFLOW, &EDIT_SKILL, &EDIT_AGENT],
};

const EDIT_SPEC: CommandSpec = CommandSpec {
    name: "spec",
    aliases: &[],
    help: "Edit an existing work item spec.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "ref",
        help: "Work item reference: configured-source value, local:<N> or Markdown path, github:<N>, #<N>, owner/repo#N, GitHub URL, or configured HTTP provider reference.",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: EDIT_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::edit,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: true,
    subcommands: &[],
};

macro_rules! edit_artifact {
    ($const_name:ident, $name:literal, $help:literal) => {
        const $const_name: CommandSpec = CommandSpec {
            name: $name,
            aliases: &[],
            help: $help,
            long_help: None,
            arguments: &[ArgumentSpec {
                name: "ref",
                help: "Existing artifact name or path.",
                kind: ArgumentKind::String,
                optional: false,
            }],
            flags: EDIT_ARTIFACT_FLAGS,
            api_allowed: false,
            build: crate::command::dispatch::build::edit,
            gateway_need: GatewayNeed::None,
            requires_container_tier: false,
            requires_runtime: true,
            opens_tui_when_bare: false,
            internal: false,
            relay_allowed: true,
            subcommands: &[],
        };
    };
}

edit_artifact!(EDIT_WORKFLOW, "workflow", "Edit an existing workflow.");
edit_artifact!(EDIT_SKILL, "skill", "Edit an existing skill.");
edit_artifact!(
    EDIT_AGENT,
    "agent",
    "Edit an existing custom agent definition."
);

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn edit_flags_parse_and_creation_flags_are_absent() {
        let catalogue = CommandCatalogue::get();
        for kind in ["spec", "workflow", "skill", "agent"] {
            let path = ["edit", kind];
            let spec = catalogue.lookup(&path).unwrap();
            assert_eq!(spec.arguments.len(), 1);
            assert_eq!(spec.arguments[0].name, "ref");
            assert!(!spec.arguments[0].optional);
            for name in [
                "interview",
                "amend",
                "headless",
                "prompt",
                "json",
                "allow-docker",
                "agent",
                "model",
                "overlay",
                "agent-flag",
            ] {
                assert!(spec.find_flag(name).is_some(), "{kind}: {name}");
            }
            assert_eq!(spec.find_flag("global").is_some(), kind != "spec");
            for name in [
                "from",
                "force",
                "format",
                "pull",
                "pull-all",
                "subdir",
                "work-item",
            ] {
                assert!(spec.find_flag(name).is_none(), "{kind}: {name}");
            }
            let parsed = catalogue
                .parse_raw_args(&path, &args(&["target", "--interview"]))
                .unwrap();
            assert_eq!(parsed.argument("ref").as_deref(), Some("target"));
            assert_eq!(parsed.flag_bool("interview"), Some(true));
            assert!(!spec.api_allowed);
            assert!(spec.relay_allowed && spec.requires_runtime);
        }
        assert!(
            catalogue
                .lookup(&["edit", "agent"])
                .unwrap()
                .requires_runtime
        );
        assert!(!catalogue.lookup(&["edit"]).unwrap().relay_allowed);
    }

    #[test]
    fn edit_requires_ref_and_rejects_interview_conflicts() {
        let catalogue = CommandCatalogue::get();
        let path = ["edit", "spec"];
        // Raw parsing leaves a missing positional to the resolver; the spec
        // must declare `ref` as required so the resolver rejects it.
        let spec = catalogue.lookup(&path).unwrap();
        assert!(spec
            .arguments
            .iter()
            .any(|a| a.name == "ref" && !a.optional));
        for conflicting in ["--headless", "--json"] {
            let error = catalogue
                .parse_raw_args(&path, &args(&["42", "--interview", conflicting]))
                .unwrap_err();
            assert!(
                matches!(
                    &error,
                    crate::command::error::CommandError::InvalidFlagValue { flag, reason, .. }
                        if flag == "interview" && reason.contains("conflicts with")
                ),
                "{error:?}"
            );
        }
        let parsed = catalogue
            .parse_raw_args(&path, &args(&["42", "-n", "--amend", "--allow-docker"]))
            .unwrap();
        assert_eq!(parsed.flag_bool("headless"), Some(true));
        assert_eq!(parsed.flag_bool("allow-docker"), Some(true));
        // `--json` implies `--headless`; implication is applied when flags resolve.
        let json = spec.flags.iter().find(|f| f.long == "json").unwrap();
        assert_eq!(json.implies, &["headless"]);
    }
}
