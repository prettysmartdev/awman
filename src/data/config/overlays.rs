//! Layer 0 — the overlay grammar, its parsed types, and the merge of every
//! overlay source into one `CollectedOverlays`.
//!
//! Overlays name host directories, skills, environment variables and context
//! directories that a higher layer will mount into an agent. Deciding *which*
//! of them a run gets is config resolution: the six sources (global config,
//! repo config, `AWMAN_OVERLAYS`, CLI flags, workflow-level and per-step
//! values) merge here, beside every other config merge, so the grammar has one
//! definition and `overlays` is no longer an opaque `Vec<String>`
//! that only Layer 2 could read.
//!
//! `OverlayPermission`, `DirectorySpec` and `ContextScope` live here rather
//! than in the engine because the parser constructs them and a Layer 0 parser
//! cannot name a Layer 1 type. `engine::container::options` and
//! `engine::overlay` re-export all three for one release, so existing paths
//! still compile.

use crate::data::config::EffectiveConfig;
use crate::data::error::DataError;
use serde::{Deserialize, Serialize};

/// Read-only or read-write, as a mount is granted to an agent.
///
/// Moved here from `engine::container::options` by WI 0114 F-27: it is the
/// permission half of every parsed overlay spec, so the Layer 0 grammar has
/// to be able to name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayPermission {
    ReadOnly,
    ReadWrite,
}

impl OverlayPermission {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverlayPermission::ReadOnly => "ro",
            OverlayPermission::ReadWrite => "rw",
        }
    }
}

/// An unresolved directory overlay: the host path exactly as the user wrote
/// it (tilde already expanded), the container path, and the permission.
///
/// Moved here from `engine::overlay` by WI 0114 F-27. Resolution — canonical
/// host paths, existence checks — stays in the engine on `DirectoryOverlay`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectorySpec {
    pub host: String,
    pub container: String,
    pub permission: OverlayPermission,
}

/// Scope for a context overlay. Moved here from `engine::overlay` by WI 0114
/// F-27 so the Layer 0 grammar can construct it; the engine re-exports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextScope {
    Global,
    Repo,
    Workflow,
}

/// Specification for a skill overlay: all skills or a named one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillSpec {
    All,
    Named(String),
}

/// A parsed context overlay specification (scope + permission).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextOverlaySpec {
    pub scope: ContextScope,
    pub permission: OverlayPermission,
}

/// A parsed overlay expression: directory mount, skill, env passthrough, or context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TypedOverlay {
    Directory(DirectorySpec),
    Skill(SkillSpec),
    Env(String),
    Context(ContextOverlaySpec),
}

/// Aggregated overlay information after collecting from all sources.
#[derive(Debug, Clone, Default)]
pub struct CollectedOverlays {
    pub directories: Vec<DirectorySpec>,
    pub include_all_skills: bool,
    pub named_skills: Vec<String>,
    pub env_passthrough: Vec<String>,
    pub context_overlays: Vec<ContextOverlaySpec>,
}

/// Parse a user-supplied overlay spec string in the form
/// `host:container` or `host:container:perm` (where perm is `ro` or `rw`).
///
/// Returns the parsed `DirectorySpec` or a descriptive error string on failure.
pub fn parse_overlay_spec(spec: &str) -> Result<DirectorySpec, String> {
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
/// `AWMAN_OVERLAYS` env var or config arrays.
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
            if args.is_empty() {
                return Err("skill() requires an argument; use skill(*) to mount all skills or skill(name) for a specific named skill".to_string());
            }
            if args.contains(',') {
                return Err("skill() takes one argument; use separate skill() calls for multiple named skills".to_string());
            }
            if args == "*" {
                Ok(TypedOverlay::Skill(SkillSpec::All))
            } else {
                if args.matches('/').count() > 1 {
                    return Err(format!(
                        "skill(name) supports at most one '/' (library/skill); got '{args}'"
                    ));
                }
                // Each segment must be a single, contained path component:
                // an empty, '.' or '..' segment is joined onto a host skills
                // path at mount time and would resolve somewhere the reference
                // never named (e.g. `skill(lib/..)` = the whole clone).
                if args
                    .split('/')
                    .any(|segment| segment.is_empty() || segment == "." || segment == "..")
                {
                    return Err(format!(
                        "skill(name) segments must not be empty, '.' or '..'; got '{args}'"
                    ));
                }
                Ok(TypedOverlay::Skill(SkillSpec::Named(args.to_string())))
            }
        }
        "skills" => {
            Err("skills() has been removed; use skill(*) to mount all skills or skill(name) for a specific named skill".to_string())
        }
        "ssh" => {
            if !args.is_empty() {
                return Err("ssh() takes no arguments".to_string());
            }
            let home = dirs::home_dir().unwrap_or_default();
            let ssh_host = home.join(".ssh").to_string_lossy().into_owned();
            Ok(TypedOverlay::Directory(DirectorySpec {
                host: ssh_host,
                container: "~/.ssh".to_string(),
                permission: OverlayPermission::ReadOnly,
            }))
        }
        "env" => {
            if args.is_empty() {
                return Err("env() requires an argument".to_string());
            }
            if args.contains(',') {
                return Err("env() takes one argument; use separate env() calls for multiple vars".to_string());
            }
            // The argument becomes an environment variable *name*: it is emitted
            // as `-e NAME` and, for a squad daemon, set on the spawned container
            // CLI's own environment via `Command::env`. A name containing `=`
            // would produce a malformed entry there rather than a passthrough,
            // and one containing a NUL would fail the spawn. Refusing it at the
            // front door means a bad task is rejected when it is created rather
            // than discovered at its first scheduled run, hours later.
            let name = args;
            let valid = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return Err(format!(
                    "env() argument {name:?} is not a valid environment variable name \
                     (letters, digits and underscore, not starting with a digit)"
                ));
            }
            Ok(TypedOverlay::Env(name.to_string()))
        }
        "context" => {
            if args.is_empty() {
                return Err("context() requires a scope argument: context(global), context(repo), or context(workflow)".to_string());
            }
            let parts: Vec<&str> = args.splitn(2, ':').collect();
            let scope_str = parts[0].trim();
            let scope = match scope_str {
                "global" => ContextScope::Global,
                "repo" => ContextScope::Repo,
                "workflow" => ContextScope::Workflow,
                other => {
                    return Err(format!(
                        "unknown context scope '{other}' in '{expr}'; \
                         supported scopes: global, repo, workflow"
                    ));
                }
            };
            let permission = if let Some(perm_str) = parts.get(1).map(|s| s.trim()) {
                match perm_str {
                    "ro" => OverlayPermission::ReadOnly,
                    "rw" => OverlayPermission::ReadWrite,
                    other => {
                        return Err(format!(
                            "unknown permission '{other}' in '{expr}'; expected 'ro' or 'rw'"
                        ));
                    }
                }
            } else {
                OverlayPermission::ReadWrite
            };
            Ok(TypedOverlay::Context(ContextOverlaySpec { scope, permission }))
        }
        _ => Err(format!(
            "unknown overlay type '{tag}' in '{expr}'; supported types: dir, skill, ssh, env, context"
        )),
    }
}

fn parse_dir_overlay_args(args: &str, full_expr: &str) -> Result<DirectorySpec, String> {
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

/// Where a merged `overlays` entry came from, for error messages: the
/// override channel names itself (`--set overlays=…`, `AWMAN_OVERLAYS`);
/// anything else is a config file.
fn overlay_source(ec: &EffectiveConfig, spec: &str) -> &'static str {
    let names = |pairs: &crate::data::config::overrides::ConfigOverrides| {
        pairs.iter().any(|(path, value)| {
            path == "overlays"
                && value
                    .as_array()
                    .is_some_and(|items| items.iter().any(|v| v.as_str() == Some(spec)))
        })
    };
    let layers = ec.layers();
    if names(&layers.set) {
        "--set overlays"
    } else if names(&layers.env) {
        crate::data::config::env::AWMAN_OVERLAYS
    } else {
        "the `overlays` config key"
    }
}

impl EffectiveConfig {
    /// Collect every overlay source into one `CollectedOverlays`.
    ///
    /// Precedence, lowest first: global config, repo config, `AWMAN_OVERLAYS`,
    /// `cli_typed_overlays`, workflow-level overlays, per-step overlays.
    /// Directory mounts accumulate in that order; skills, env passthroughs and
    /// context scopes de-duplicate, so the first source to name one wins.
    ///
    /// Replaces the Layer 2 free function `collect_all_overlay_specs`
    /// (WI 0114 F-27). Callers holding a `Session` pass
    /// `session.effective_config()`.
    pub fn collected_overlays(
        &self,
        cli_typed_overlays: Vec<TypedOverlay>,
        workflow_overlays: Option<&[String]>,
        step_overlays: Option<&[String]>,
    ) -> Result<CollectedOverlays, DataError> {
        let ec = self;
        let mut dirs = Vec::new();
        let mut include_all_skills = false;
        let mut named_skills: Vec<String> = Vec::new();
        let mut env_passthrough: Vec<String> = Vec::new();
        let mut context_overlays: Vec<ContextOverlaySpec> = Vec::new();

        let mut process_typed = |typed: TypedOverlay| match typed {
            TypedOverlay::Directory(spec) => dirs.push(spec),
            TypedOverlay::Skill(SkillSpec::All) => include_all_skills = true,
            TypedOverlay::Skill(SkillSpec::Named(name)) => {
                if !named_skills.contains(&name) {
                    named_skills.push(name);
                }
            }
            TypedOverlay::Env(var) => {
                if !env_passthrough.contains(&var) {
                    env_passthrough.push(var);
                }
            }
            TypedOverlay::Context(spec) => {
                if !context_overlays.iter().any(|c| c.scope == spec.scope) {
                    context_overlays.push(spec);
                }
            }
        };

        // 1-3. Config overlays: global, repo, task, env, `--set` — additive
        // across layers, already merged and de-duplicated by `EffectiveConfig`.
        for s in ec.config().overlays.iter().flatten() {
            let parsed = parse_overlay_list(s).map_err(|reason| DataError::InvalidOverlaySpec {
                spec: s.clone(),
                reason: format!("{reason} (from {})", overlay_source(ec, s)),
            })?;
            for typed in parsed {
                process_typed(typed);
            }
        }

        // 4. CLI flag overlays (highest priority).
        for typed in cli_typed_overlays {
            process_typed(typed);
        }

        // 5. Workflow-level overlays (between CLI and step).
        if let Some(wf_strs) = workflow_overlays {
            for s in wf_strs {
                let parsed =
                    parse_overlay_list(s).map_err(|reason| DataError::InvalidOverlaySpec {
                        spec: s.clone(),
                        reason,
                    })?;
                for typed in parsed {
                    process_typed(typed);
                }
            }
        }

        // 6. Per-step overlays (highest priority).
        if let Some(step_strs) = step_overlays {
            for s in step_strs {
                let parsed =
                    parse_overlay_list(s).map_err(|reason| DataError::InvalidOverlaySpec {
                        spec: s.clone(),
                        reason,
                    })?;
                for typed in parsed {
                    process_typed(typed);
                }
            }
        }

        Ok(CollectedOverlays {
            directories: dirs,
            include_all_skills,
            named_skills,
            env_passthrough,
            context_overlays,
        })
    }
}

// ─── WI-0087: context() overlay parsing ──────────────────────────────────────

// ─── WI-0087: EffectiveConfig::collected_overlays with context overlays ─────────────────
