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
pub mod claws;
pub mod command_trait;
pub mod config;
pub mod download;
pub mod exec_prompt;
pub mod exec_workflow;
pub mod headless;
pub mod implement;
pub mod implement_prompts;
pub mod init;
pub mod mount_scope;
pub mod new;
pub mod ready;
pub mod remote;
pub(super) mod remote_client;
pub mod specs;
pub mod status;
pub mod status_tips;
pub mod worktree_lifecycle;

pub use command_trait::Command;

/// Parse a user-supplied overlay spec string in the form
/// `host:container` or `host:container:perm` (where perm is `ro` or `rw`).
///
/// Returns the parsed `DirectorySpec` or a descriptive error string on failure.
pub fn parse_overlay_spec(
    spec: &str,
) -> Result<crate::engine::overlay::DirectorySpec, String> {
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
/// Grammar: `dir(host:container[:perm])` expressions separated by commas.
/// Commas inside parentheses are ignored (paren-aware splitting).
pub fn parse_overlay_list(
    input: &str,
) -> Result<Vec<crate::engine::overlay::DirectorySpec>, String> {
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

/// Parse a single typed overlay expression like `dir(/host:/container:ro)`.
fn parse_single_typed_overlay(
    expr: &str,
) -> Result<crate::engine::overlay::DirectorySpec, String> {
    let open = expr
        .find('(')
        .ok_or_else(|| format!("malformed overlay expression (missing '('): '{expr}'"))?;
    let close = expr
        .rfind(')')
        .ok_or_else(|| format!("malformed overlay expression (missing ')'): '{expr}'"))?;
    if close <= open {
        return Err(format!("malformed overlay expression (parentheses out of order): '{expr}'"));
    }
    let tag = expr[..open].trim();
    let args = expr[open + 1..close].trim();
    match tag {
        "dir" => parse_dir_overlay_args(args, expr),
        _ => Err(format!(
            "unknown overlay type '{tag}' in '{expr}'; supported types: dir"
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
        return Err(format!("empty arguments in overlay expression: '{full_expr}'"));
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
            return Err(format!(
                "expected 'host:container[:perm]' in '{full_expr}'"
            ));
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
    // Expand ~ in host path.
    let host_expanded = if host.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            let rest = host.strip_prefix("~/").unwrap_or(&host[1..]);
            home.join(rest).to_string_lossy().to_string()
        } else {
            host.to_string()
        }
    } else {
        host.to_string()
    };
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
pub fn collect_all_overlay_specs(
    session: &crate::data::session::Session,
    cli_overlays: Vec<crate::engine::overlay::DirectorySpec>,
) -> Vec<crate::engine::overlay::DirectorySpec> {
    let ec = session.effective_config();
    let mut specs = Vec::new();

    // 1. Global config overlays (lowest priority).
    if let Some(overlays) = ec.global().overlays.as_ref() {
        if let Some(dirs) = overlays.directories.as_ref() {
            for d in dirs {
                specs.push(config_overlay_to_spec(d));
            }
        }
    }

    // 2. Repo config overlays.
    if let Some(overlays) = ec.repo().overlays.as_ref() {
        if let Some(dirs) = overlays.directories.as_ref() {
            for d in dirs {
                specs.push(config_overlay_to_spec(d));
            }
        }
    }

    // 3. AMUX_OVERLAYS env var.
    if let Some(env_str) = ec.env().overlays() {
        if let Ok(parsed) = parse_overlay_list(env_str) {
            specs.extend(parsed);
        }
    }

    // 4. CLI flag overlays (highest priority).
    specs.extend(cli_overlays);

    specs
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
