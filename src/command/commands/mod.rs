//! `src/command/commands/` — one struct per amux command.
//!
//! Each module contains the `*Command` struct (owning every flag value and
//! engine reference it needs), its `*CommandFrontend` trait (defining the
//! exact user-input methods that command requires), and the
//! `Command` impl whose `run_with_frontend(frontend) -> *Outcome` body holds
//! all of the command's business logic.

pub mod agent_auth;
pub mod agent_setup;
pub mod auth;
pub mod chat;
pub mod command_trait;
pub mod config;
pub mod download;
pub mod exec_prompt;
pub mod exec_workflow;
pub mod headless;
pub mod init;
pub mod mount_scope;
pub mod new;
pub mod prompt_templates;
pub mod ready;
pub mod remote;
pub(super) mod remote_client;
pub mod specs;
pub mod status;
pub mod status_tips;
pub mod worktree_lifecycle;

pub use command_trait::Command;

/// A parsed overlay expression: either a directory mount or a skills overlay.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedOverlay {
    Directory(crate::engine::overlay::DirectorySpec),
    Skill,
}

/// Parse a user-supplied overlay spec string in the form
/// `host:container` or `host:container:perm` (where perm is `ro` or `rw`).
///
/// Returns the parsed `DirectorySpec` or a descriptive error string on failure.
pub fn parse_overlay_spec(spec: &str) -> Result<crate::engine::overlay::DirectorySpec, String> {
    use crate::engine::container::options::OverlayPermission;
    use crate::engine::overlay::DirectorySpec;

    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(format!(
            "expected 'host:container' or 'host:container:perm', got '{spec}'"
        ));
    }
    let host = parts[0].to_string();
    if host.is_empty() {
        return Err("host path must not be empty".to_string());
    }
    let container = parts[1].to_string();
    if container.is_empty() {
        return Err("container path must not be empty".to_string());
    }
    if !container.starts_with('/') {
        return Err(format!("container path '{container}' must be absolute"));
    }
    let permission = match parts.get(2).copied() {
        None | Some("rw") | Some("") => OverlayPermission::ReadWrite,
        Some("ro") => OverlayPermission::ReadOnly,
        Some(other) => {
            return Err(format!(
                "unknown permission '{other}'; expected 'ro' or 'rw'"
            ));
        }
    };
    Ok(DirectorySpec {
        host,
        container,
        permission,
    })
}

/// Parse a comma-separated list of typed overlay expressions from the
/// `AMUX_OVERLAYS` env var or config arrays.
///
/// Grammar: `dir(host:container[:perm])` or `skill()` expressions separated
/// by commas. Bare `host:container[:perm]` strings (no type tag) are accepted
/// as legacy shorthand for `dir(...)`. Commas inside parentheses are ignored
/// (paren-aware splitting).
pub fn parse_overlay_list(input: &str) -> Result<Vec<TypedOverlay>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(vec![]);
    }
    let mut results = Vec::new();
    for expr in split_top_level_commas(input) {
        let expr = expr.trim();
        if expr.is_empty() {
            continue;
        }
        results.push(parse_single_typed_overlay(expr)?);
    }
    Ok(results)
}

/// Split on commas not inside parentheses.
fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                results.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    results.push(&input[start..]);
    results
}

/// Parse a single typed overlay expression like `dir(/host:/container:ro)`
/// or `skill()`. If the input has no parentheses, it is treated as a legacy
/// bare path spec (`host:container[:perm]`).
fn parse_single_typed_overlay(expr: &str) -> Result<TypedOverlay, String> {
    if !expr.contains('(') {
        return parse_overlay_spec(expr).map(TypedOverlay::Directory);
    }
    let open = expr
        .find('(')
        .ok_or_else(|| format!("malformed overlay expression (missing '('): '{expr}'"))?;
    let close = expr
        .rfind(')')
        .ok_or_else(|| format!("malformed overlay expression (missing ')'): '{expr}'"))?;
    if close <= open {
        return Err(format!(
            "malformed overlay expression (parentheses out of order): '{expr}'"
        ));
    }
    let tag = expr[..open].trim();
    let args = expr[open + 1..close].trim();
    match tag {
        "dir" => parse_dir_overlay_args(args, expr).map(TypedOverlay::Directory),
        "skill" => {
            if !args.is_empty() {
                return Err(format!(
                    "'skill()' takes no arguments, got '{args}' in '{expr}'"
                ));
            }
            Ok(TypedOverlay::Skill)
        }
        _ => Err(format!(
            "unknown overlay type '{tag}' in '{expr}'; supported types: dir, skill"
        )),
    }
}

fn parse_dir_overlay_args(
    args: &str,
    full_expr: &str,
) -> Result<crate::engine::overlay::DirectorySpec, String> {
    use crate::engine::container::options::OverlayPermission;
    use crate::engine::overlay::DirectorySpec;

    if args.is_empty() {
        return Err(format!(
            "empty arguments in overlay expression: '{full_expr}'"
        ));
    }
    let parts: Vec<&str> = args.splitn(3, ':').collect();
    let (host_str, container_str, perm_str) = match parts.len() {
        2 => (parts[0], parts[1], None),
        3 => {
            let candidate = parts[2].trim();
            if candidate == "ro" || candidate == "rw" {
                (parts[0], parts[1], Some(candidate))
            } else {
                return Err(format!(
                    "invalid permission '{candidate}' in '{full_expr}'; expected 'ro' or 'rw'"
                ));
            }
        }
        _ => {
            return Err(format!("expected 'host:container[:perm]' in '{full_expr}'"));
        }
    };
    let host = host_str.trim();
    let container = container_str.trim();
    if host.is_empty() {
        return Err(format!("empty host path in '{full_expr}'"));
    }
    if container.is_empty() {
        return Err(format!("empty container path in '{full_expr}'"));
    }
    let permission = match perm_str {
        Some("ro") => OverlayPermission::ReadOnly,
        _ => OverlayPermission::ReadWrite,
    };
    let host_expanded = crate::data::fs::OverlayPathResolver::expand_tilde(host)
        .to_string_lossy()
        .into_owned();
    Ok(DirectorySpec {
        host: host_expanded,
        container: container.to_string(),
        permission,
    })
}

/// Convert a `DirectoryOverlayConfig` from JSON config into a `DirectorySpec`.
pub fn config_overlay_to_spec(
    cfg: &crate::data::config::repo::DirectoryOverlayConfig,
) -> crate::engine::overlay::DirectorySpec {
    use crate::engine::container::options::OverlayPermission;
    use crate::engine::overlay::DirectorySpec;

    let permission = match cfg.permission.as_deref() {
        Some("rw") => OverlayPermission::ReadWrite,
        _ => OverlayPermission::ReadOnly,
    };
    DirectorySpec {
        host: cfg.host.clone(),
        container: cfg.container.clone(),
        permission,
    }
}

/// Collect all directory overlays from effective config sources (global config,
/// repo config, AMUX_OVERLAYS env var) and merge with CLI flag overlays.
///
/// Returns `(directory_specs, skills_enabled)` where `skills_enabled` is true
/// when any source (config, env var, or CLI) requests the skills overlay.
pub fn collect_all_overlay_specs(
    session: &crate::data::session::Session,
    cli_typed_overlays: Vec<TypedOverlay>,
) -> (Vec<crate::engine::overlay::DirectorySpec>, bool) {
    let ec = session.effective_config();
    let mut specs = Vec::new();
    let mut skills_enabled = false;

    // 1. Global config overlays (lowest priority).
    if let Some(overlays) = ec.global().overlays.as_ref() {
        if let Some(dirs) = overlays.directories.as_ref() {
            for d in dirs {
                specs.push(config_overlay_to_spec(d));
            }
        }
        if overlays.skills == Some(true) {
            skills_enabled = true;
        }
    }

    // 2. Repo config overlays.
    if let Some(overlays) = ec.repo().overlays.as_ref() {
        if let Some(dirs) = overlays.directories.as_ref() {
            for d in dirs {
                specs.push(config_overlay_to_spec(d));
            }
        }
        if overlays.skills == Some(true) {
            skills_enabled = true;
        }
    }

    // 3. AMUX_OVERLAYS env var.
    if let Some(env_str) = ec.env().overlays() {
        if let Ok(parsed) = parse_overlay_list(env_str) {
            for typed in parsed {
                match typed {
                    TypedOverlay::Directory(spec) => specs.push(spec),
                    TypedOverlay::Skill => skills_enabled = true,
                }
            }
        }
    }

    // 4. CLI flag overlays (highest priority).
    for typed in cli_typed_overlays {
        match typed {
            TypedOverlay::Directory(spec) => specs.push(spec),
            TypedOverlay::Skill => skills_enabled = true,
        }
    }

    (specs, skills_enabled)
}

#[cfg(test)]
mod skill_parser_tests {
    use super::*;

    #[test]
    fn skill_empty_parses_to_skill_variant() {
        let result = parse_overlay_list("skill()").unwrap();
        assert_eq!(result, vec![TypedOverlay::Skill]);
    }

    #[test]
    fn skill_with_args_returns_error_takes_no_arguments() {
        let err = parse_overlay_list("skill(anything)").unwrap_err();
        assert!(
            err.contains("takes no arguments"),
            "error must mention 'takes no arguments'; got: {err}"
        );
    }

    #[test]
    fn skill_and_dir_in_comma_list_produces_both_variants() {
        let result = parse_overlay_list("skill(),dir(/host:/container:ro)").unwrap();
        assert_eq!(result.len(), 2, "expected 2 overlays; got {result:?}");
        assert!(
            matches!(result[0], TypedOverlay::Skill),
            "first entry must be Skill; got {result:?}"
        );
        assert!(
            matches!(result[1], TypedOverlay::Directory(_)),
            "second entry must be Directory; got {result:?}"
        );
    }

    #[test]
    fn unknown_tag_error_lists_dir_and_skill_as_valid_types() {
        let err = parse_overlay_list("foobar(/x:/y)").unwrap_err();
        assert!(
            err.contains("dir"),
            "error must mention 'dir' as a supported type; got: {err}"
        );
        assert!(
            err.contains("skill"),
            "error must mention 'skill' as a supported type; got: {err}"
        );
    }
}

#[cfg(test)]
mod collect_overlay_specs_tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AMUX_CONFIG_HOME, AMUX_OVERLAYS};
    use crate::data::config::global::GlobalConfig;
    use crate::data::config::repo::{OverlaysConfig, RepoConfig};
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};

    fn open_session(git_root: &std::path::Path, env: EnvSnapshot) -> Session {
        let resolver = StaticGitRootResolver::new(git_root);
        let opts = SessionOpenOptions {
            flags: Default::default(),
            env: Some(env),
            available_agents: None,
        };
        Session::open(git_root.to_path_buf(), &resolver, opts).unwrap()
    }

    #[test]
    fn skills_enabled_when_repo_config_has_skills_true() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(OverlaysConfig {
                skills: Some(true),
                directories: None,
            }),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AMUX_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let (_, skills_enabled) = collect_all_overlay_specs(&session, vec![]);
        assert!(skills_enabled, "skills must be enabled from repo config");
    }

    #[test]
    fn skills_enabled_when_global_config_has_skills_true() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(OverlaysConfig {
                skills: Some(true),
                directories: None,
            }),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AMUX_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        let session = open_session(git_tmp.path(), env);

        let (_, skills_enabled) = collect_all_overlay_specs(&session, vec![]);
        assert!(skills_enabled, "skills must be enabled from global config");
    }

    #[test]
    fn skills_enabled_when_amux_overlays_env_contains_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AMUX_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AMUX_OVERLAYS, "skill()"),
        ]);
        let session = open_session(tmp.path(), env);

        let (_, skills_enabled) = collect_all_overlay_specs(&session, vec![]);
        assert!(
            skills_enabled,
            "skills must be enabled when AMUX_OVERLAYS contains skill()"
        );
    }

    #[test]
    fn skills_enabled_when_cli_typed_overlays_contains_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AMUX_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let (_, skills_enabled) = collect_all_overlay_specs(&session, vec![TypedOverlay::Skill]);
        assert!(
            skills_enabled,
            "skills must be enabled from CLI TypedOverlay::Skill"
        );
    }

    #[test]
    fn skills_disabled_when_no_source_enables_it() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AMUX_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let (_, skills_enabled) = collect_all_overlay_specs(&session, vec![]);
        assert!(
            !skills_enabled,
            "skills must be disabled when no source sets it"
        );
    }

    #[test]
    fn skills_enabled_is_additive_or_single_source_sufficient() {
        // Only global config has skills=true; repo config and CLI do not.
        // skills_enabled must still be true — OR semantics, not AND.
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(OverlaysConfig {
                skills: Some(true),
                directories: None,
            }),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AMUX_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        // Repo config has no overlays; no CLI TypedOverlay::Skill.
        let session = open_session(git_tmp.path(), env);

        let (_, skills_enabled) = collect_all_overlay_specs(&session, vec![]);
        assert!(
            skills_enabled,
            "a single source (global config) must be sufficient to enable skills (additive OR)"
        );
    }
}

#[cfg(test)]
mod overlay_spec_tests {
    use super::*;
    use crate::engine::container::options::OverlayPermission;

    #[test]
    fn parse_overlay_spec_host_container_default_rw() {
        let spec = parse_overlay_spec("/host/path:/container/path").unwrap();
        assert_eq!(spec.host, "/host/path");
        assert_eq!(spec.container, "/container/path");
        assert_eq!(spec.permission, OverlayPermission::ReadWrite);
    }

    #[test]
    fn parse_overlay_spec_with_ro_permission() {
        let spec = parse_overlay_spec("/host/path:/container/path:ro").unwrap();
        assert_eq!(spec.permission, OverlayPermission::ReadOnly);
    }

    #[test]
    fn parse_overlay_spec_with_rw_permission() {
        let spec = parse_overlay_spec("/host/path:/container/path:rw").unwrap();
        assert_eq!(spec.permission, OverlayPermission::ReadWrite);
    }

    #[test]
    fn parse_overlay_spec_missing_container_returns_error() {
        let result = parse_overlay_spec("/host/only");
        assert!(result.is_err(), "must error when container path is missing");
    }

    #[test]
    fn parse_overlay_spec_relative_container_path_returns_error() {
        let result = parse_overlay_spec("/host/path:relative/path");
        assert!(result.is_err(), "must error for relative container path");
    }

    #[test]
    fn parse_overlay_spec_unknown_permission_returns_error() {
        let result = parse_overlay_spec("/host:/container:rx");
        assert!(result.is_err(), "must error for unknown permission 'rx'");
    }

    #[test]
    fn parse_overlay_spec_empty_host_returns_error() {
        let result = parse_overlay_spec(":/container/path");
        assert!(result.is_err(), "must error for empty host path");
    }
}
