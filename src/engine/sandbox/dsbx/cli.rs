//! Every `sbx` argv awman issues, in one place (contract §5).
//!
//! Each function returns the arguments after the `sbx` program name. Keeping
//! them pure and side by side means a correction found during live
//! qualification (the UNVERIFIED rows of the contract) is made in exactly one
//! function, and the unit tests below pin every form.
//!
//! `--no-input` is a global option and goes directly after `sbx` on every
//! non-interactive call that the contract marks with it. The interactive
//! `sbx run`, `sbx stop` and the attach shell do not carry it.

use std::path::Path;

use crate::engine::agent::sbx_contract::{
    admit_cli, SbxCliAdmission, SbxCliVersion, SbxCreateSelector, GUEST_APPLY, LAUNCH_ID_ENV,
};
use crate::engine::error::EngineError;

const NO_INPUT: &str = "--no-input";

fn argv<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// 5.1 `sbx --version`. Prints the client version without starting the
/// daemon (DOC RN v0.51.0).
pub(super) fn version_argv() -> Vec<String> {
    argv(["--version"])
}

/// 5.2 `sbx --no-input ls --json`.
pub(super) fn ls_argv() -> Vec<String> {
    argv([NO_INPUT, "ls", "--json"])
}

/// 5.3 `sbx --no-input kit validate <kit_dir> [--offline]`. `--offline`
/// skips daemon policy checks and needs no sign-in; it validates the local
/// artifact only, never the effective composition.
pub(super) fn kit_validate_argv(kit_dir: &Path, offline: bool) -> Vec<String> {
    let mut args = argv([NO_INPUT, "kit", "validate"]);
    args.push(kit_dir.display().to_string());
    if offline {
        args.push("--offline".into());
    }
    args
}

/// 5.4a/5.4b `sbx --no-input create --name <name> [--kit <mixin>] [--memory
/// <N>g] <workload|kit_dir> <workspace>`.
///
/// A published workload is selected by its built-in name and the awman
/// mixin is added with `--kit`; an owned workload is selected by its kit
/// directory as the positional agent. The workspace is always passed: it is
/// the approved Git root, never a parent of it.
pub(super) fn create_argv(
    name: &str,
    selector: &SbxCreateSelector,
    memory_gb: Option<u32>,
    workspace: &Path,
) -> Vec<String> {
    let mut args = argv([NO_INPUT, "create", "--name"]);
    args.push(name.to_string());
    if let SbxCreateSelector::PublishedWithMixin { mixin_dir, .. } = selector {
        args.push("--kit".into());
        args.push(mixin_dir.display().to_string());
    }
    if let Some(mem) = memory_gb {
        args.push("--memory".into());
        args.push(format!("{mem}g"));
    }
    match selector {
        SbxCreateSelector::PublishedWithMixin { workload, .. } => args.push(workload.clone()),
        SbxCreateSelector::OwnedWorkload { kit_dir } => args.push(kit_dir.display().to_string()),
    }
    args.push(workspace.display().to_string());
    args
}

/// 5.5 `sbx --no-input exec -i -u root <name> /usr/local/lib/awman/awman-apply`;
/// the apply document goes on stdin.
pub(super) fn apply_argv(name: &str) -> Vec<String> {
    let mut args = argv([NO_INPUT, "exec", "-i", "-u", "root"]);
    args.push(name.to_string());
    args.push(GUEST_APPLY.to_string());
    args
}

/// 5.6 `sbx run --name <name> -e AWMAN_LAUNCH_ID [-e KEY]... [-- TAIL...]`.
///
/// Only bare `-e KEY` names: values are set in the environment of the `sbx
/// run` child, so none appears in argv. The tail is the agent's arguments
/// after the workload entrypoint (post-v0.53.0 semantics: the entrypoint's
/// own arguments are preserved, so nothing in the tail repeats them).
/// UNVERIFIED-V4: re-attach by `--name`, `-e KEY` reaching the agent, and the
/// tail reaching the entrypoint. Correct them here.
pub(super) fn run_argv(name: &str, env_keys: &[String], tail: &[String]) -> Vec<String> {
    let mut args = argv(["run", "--name"]);
    args.push(name.to_string());
    args.push("-e".into());
    args.push(LAUNCH_ID_ENV.into());
    for key in env_keys {
        if key == LAUNCH_ID_ENV {
            continue;
        }
        args.push("-e".into());
        args.push(key.clone());
    }
    if !tail.is_empty() {
        args.push("--".into());
        args.extend(tail.iter().cloned());
    }
    args
}

/// 5.7 `sbx --no-input exec <name> <binary> --version`.
pub(super) fn agent_version_argv(name: &str, binary: &str) -> Vec<String> {
    let mut args = argv([NO_INPUT, "exec"]);
    args.push(name.to_string());
    args.push(binary.to_string());
    args.push("--version".into());
    args
}

/// 5.8 `sbx --no-input inspect <name> --json`.
pub(super) fn inspect_argv(name: &str) -> Vec<String> {
    let mut args = argv([NO_INPUT, "inspect"]);
    args.push(name.to_string());
    args.push("--json".into());
    args
}

/// 5.9 `sbx stop <name>`.
pub(super) fn stop_argv(name: &str) -> Vec<String> {
    let mut args = argv(["stop"]);
    args.push(name.to_string());
    args
}

/// 5.11 `sbx secret set <service> --sandbox <name> --force`; the value and a
/// newline go on stdin. Never `--token` (argv), never global scope, never
/// `--password-stdin` (a registry-only flag).
pub(super) fn secret_set_argv(service: &str, sandbox: &str) -> Vec<String> {
    let mut args = argv(["secret", "set"]);
    args.push(service.to_string());
    args.push("--sandbox".into());
    args.push(sandbox.to_string());
    args.push("--force".into());
    args
}

/// 5.12 `sbx --no-input secret ls --sandbox <name> --service <service> --json`.
pub(super) fn secret_ls_sandbox_argv(service: &str, sandbox: &str) -> Vec<String> {
    let mut args = argv([NO_INPUT, "secret", "ls", "--sandbox"]);
    args.push(sandbox.to_string());
    args.push("--service".into());
    args.push(service.to_string());
    args.push("--json".into());
    args
}

/// 5.12 `sbx --no-input secret ls --global --service <service> --json`. A
/// read-only probe: awman never writes a global secret.
pub(super) fn secret_ls_global_argv(service: &str) -> Vec<String> {
    let mut args = argv([NO_INPUT, "secret", "ls", "--global", "--service"]);
    args.push(service.to_string());
    args.push("--json".into());
    args
}

/// Map the CLI admission of `sbx --version` output to the launch decision.
/// Only an admitted version may continue; there is no fallback.
pub(super) fn admission_result(version_stdout: &str) -> Result<SbxCliVersion, EngineError> {
    match admit_cli(version_stdout) {
        SbxCliAdmission::Admitted(version) => Ok(version),
        SbxCliAdmission::TooOld { found, minimum } => Err(EngineError::SbxCliIncompatible {
            found: found.to_string(),
            minimum: minimum.to_string(),
        }),
        SbxCliAdmission::KnownBad { found } => Err(EngineError::SbxCliIncompatible {
            found: format!("{found} (a release known not to work with awman)"),
            minimum: crate::engine::agent::sbx_contract::SBX_MIN_CLI.to_string(),
        }),
        SbxCliAdmission::Unparseable { raw } => Err(EngineError::SbxCliVersionUnreadable { raw }),
    }
}

/// Run `sbx --version` quietly and admit the client. A pre-0.51 CLI rejects
/// `--version`; its non-zero exit is read like any unreadable output.
pub(super) fn admit_installed_cli(
    snapshot: &crate::data::config::env::EnvSnapshot,
) -> Result<SbxCliVersion, EngineError> {
    let out = super::spawn::SbxCommand::new(version_argv())
        .in_env(snapshot)
        .run_quiet()?;
    if !out.success() {
        let raw = format!("{} {}", out.stdout.trim(), out.stderr.trim());
        return Err(EngineError::SbxCliVersionUnreadable {
            raw: raw.trim().lines().next().unwrap_or("").to_string(),
        });
    }
    admission_result(&out.stdout)
}

/// Sandbox names in `sbx ls --json` output. UNVERIFIED-V2: accepts a
/// top-level array or `{"sandboxes": [...]}` and `name` or `Name`.
/// `None` when the output is not such JSON; the table fallback is gone.
pub(super) fn parse_ls_json(stdout: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let array = match &value {
        serde_json::Value::Array(items) => items.clone(),
        other => match (other.get("sandboxes"), other.get("Sandboxes")) {
            (Some(v), None) | (None, Some(v)) => v.as_array()?.clone(),
            _ => return None,
        },
    };
    let mut names = Vec::new();
    for item in &array {
        let name = match (item.get("name"), item.get("Name")) {
            (Some(v), None) | (None, Some(v)) => v.as_str()?,
            _ => return None,
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            || names.iter().any(|n| n == name)
        {
            return None;
        }
        names.push(name.to_string());
    }
    Some(names)
}

/// Whether `sbx secret ls --json` output lists at least one secret.
/// UNVERIFIED-V7: "present" is a non-empty JSON array, or an object with a
/// non-empty array member. Anything else (including unparsable output)
/// counts as absent, so an ambiguous listing never suppresses the warning.
pub(super) fn secret_listing_present(stdout: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Ok(serde_json::Value::Array(items)) => !items.is_empty(),
        Ok(serde_json::Value::Object(map)) => map
            .values()
            .any(|v| v.as_array().is_some_and(|a| !a.is_empty())),
        _ => false,
    }
}

/// The value of the first key named `kits` (any case) in `sbx inspect
/// --json` output, recorded as evidence of what `latest` resolved to.
pub(super) fn inspect_kits(stdout: &str) -> Option<serde_json::Value> {
    fn find(value: &serde_json::Value) -> Option<serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("kits"))
                .map(|(_, v)| v.clone())
                .or_else(|| map.values().find_map(find)),
            serde_json::Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    find(&value)
}

#[cfg(test)]
mod argv_tests {
    //! Golden argv for every `sbx` call awman makes (contract §5).
    use super::*;
    use std::path::PathBuf;

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn probe_and_lifecycle_argv_match_the_contract() {
        assert_eq!(version_argv(), s(&["--version"]));
        assert_eq!(ls_argv(), s(&["--no-input", "ls", "--json"]));
        assert_eq!(
            kit_validate_argv(Path::new("/k/awman-claude-mixin"), false),
            s(&["--no-input", "kit", "validate", "/k/awman-claude-mixin"])
        );
        assert_eq!(
            kit_validate_argv(Path::new("/k"), true),
            s(&["--no-input", "kit", "validate", "/k", "--offline"])
        );
        assert_eq!(
            apply_argv("awman-1-claude"),
            s(&[
                "--no-input",
                "exec",
                "-i",
                "-u",
                "root",
                "awman-1-claude",
                "/usr/local/lib/awman/awman-apply"
            ])
        );
        assert_eq!(
            agent_version_argv("n", "opencode"),
            s(&["--no-input", "exec", "n", "opencode", "--version"])
        );
        assert_eq!(
            inspect_argv("n"),
            s(&["--no-input", "inspect", "n", "--json"])
        );
        assert_eq!(stop_argv("n"), s(&["stop", "n"]));
    }

    #[test]
    fn create_selects_published_workload_plus_mixin_or_the_owned_kit_directory() {
        let published = SbxCreateSelector::PublishedWithMixin {
            workload: "claude".into(),
            mixin_dir: PathBuf::from("/kits/v3/awman-claude-mixin"),
        };
        assert_eq!(
            create_argv("awman-1-claude", &published, Some(8), Path::new("/repo")),
            s(&[
                "--no-input",
                "create",
                "--name",
                "awman-1-claude",
                "--kit",
                "/kits/v3/awman-claude-mixin",
                "--memory",
                "8g",
                "claude",
                "/repo"
            ])
        );
        let owned = SbxCreateSelector::OwnedWorkload {
            kit_dir: PathBuf::from("/kits/v3/awman-opencode"),
        };
        assert_eq!(
            create_argv("awman-1-opencode", &owned, None, Path::new("/repo")),
            s(&[
                "--no-input",
                "create",
                "--name",
                "awman-1-opencode",
                "/kits/v3/awman-opencode",
                "/repo"
            ])
        );
    }

    #[test]
    fn run_passes_env_names_only_and_the_tail_once_after_the_delimiter() {
        assert_eq!(
            run_argv("n", &[], &[]),
            s(&["run", "--name", "n", "-e", "AWMAN_LAUNCH_ID"])
        );
        let argv = run_argv(
            "n",
            &["FOO".into(), "AWMAN_LAUNCH_ID".into()],
            &s(&["--print", "hi"]),
        );
        assert_eq!(
            argv,
            s(&[
                "run",
                "--name",
                "n",
                "-e",
                "AWMAN_LAUNCH_ID",
                "-e",
                "FOO",
                "--",
                "--print",
                "hi"
            ])
        );
        assert_eq!(argv.iter().filter(|a| *a == "--").count(), 1);
        assert_eq!(argv.iter().filter(|a| *a == "run").count(), 1);
        assert!(argv.iter().all(|a| !a.contains('=')));
    }

    #[test]
    fn secrets_are_sandbox_scoped_with_no_value_in_argv() {
        let set = secret_set_argv("anthropic", "awman-1-claude");
        assert_eq!(
            set,
            s(&[
                "secret",
                "set",
                "anthropic",
                "--sandbox",
                "awman-1-claude",
                "--force"
            ])
        );
        for forbidden in ["-g", "--global", "--token", "-t", "--password-stdin"] {
            assert!(!set.contains(&forbidden.to_string()), "{forbidden}");
        }
        assert_eq!(
            secret_ls_sandbox_argv("anthropic", "n"),
            s(&[
                "--no-input",
                "secret",
                "ls",
                "--sandbox",
                "n",
                "--service",
                "anthropic",
                "--json"
            ])
        );
        assert_eq!(
            secret_ls_global_argv("anthropic"),
            s(&[
                "--no-input",
                "secret",
                "ls",
                "--global",
                "--service",
                "anthropic",
                "--json"
            ])
        );
    }

    #[test]
    fn admission_refuses_old_unreadable_and_accepts_newer_clients() {
        assert_eq!(
            admission_result("sbx version v0.54.1 (abc)").unwrap(),
            SbxCliVersion::new(0, 54, 1)
        );
        assert!(admission_result("v1.2.0").is_ok());
        match admission_result("sbx 0.52.0").unwrap_err() {
            EngineError::SbxCliIncompatible { found, minimum } => {
                assert_eq!(found, "0.52.0");
                assert_eq!(minimum, "0.53.0");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            admission_result("unknown flag: --version"),
            Err(EngineError::SbxCliVersionUnreadable { .. })
        ));
    }

    #[test]
    fn tolerant_parsers_read_the_documented_shapes() {
        assert_eq!(
            parse_ls_json(r#"[{"name":"a"},{"Name":"b"}]"#),
            Some(s(&["a", "b"]))
        );
        assert_eq!(
            parse_ls_json(r#"{"sandboxes":[{"name":"a"}]}"#),
            Some(s(&["a"]))
        );
        assert_eq!(parse_ls_json("[]"), Some(vec![]));
        for unknown in [
            r#"[{"sandbox_name":"awman-owned"}]"#,
            r#"[{"name":"valid"},{"Name":4}]"#,
            r#"[{"name":"a","Name":"b"}]"#,
            r#"[{"name":""}]"#,
            r#"[{"name":"awman/invalid"}]"#,
            r#"[{"name":"awman-\u0000"}]"#,
            r#"[{"name":"a"},{"name":"a"}]"#,
        ] {
            assert_eq!(parse_ls_json(unknown), None, "{unknown}");
        }
        assert_eq!(parse_ls_json("NAME STATUS\nawman-x running"), None);
        assert!(secret_listing_present(r#"[{"service":"anthropic"}]"#));
        assert!(secret_listing_present(r#"{"secrets":[{"service":"x"}]}"#));
        assert!(!secret_listing_present("[]"));
        assert!(!secret_listing_present(r#"{"secrets":[]}"#));
        assert!(!secret_listing_present("anthropic"));
        assert_eq!(
            inspect_kits(r#"{"config":{"Kits":["docker.io/sbx/claude@sha256:1"]}}"#),
            Some(serde_json::json!(["docker.io/sbx/claude@sha256:1"]))
        );
        assert_eq!(inspect_kits("not json"), None);
    }
}
