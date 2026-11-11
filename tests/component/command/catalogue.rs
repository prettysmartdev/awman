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
