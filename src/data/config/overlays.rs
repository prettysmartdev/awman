//! Layer 0 — the overlay grammar, its parsed types, and the merge of every
//! overlay source into one `CollectedOverlays`.
//!
//! Overlays name host directories, skills, environment variables and context
//! directories that a higher layer will mount into an agent. Deciding *which*
//! of them a run gets is config resolution: the six sources (global config,
//! repo config, `AWMAN_OVERLAYS`, CLI flags, workflow-level and per-step
//! values) merge here, beside every other config merge, so the grammar has one
//! definition and `RepoConfig.overlays` is no longer an opaque `Vec<String>`
//! that only Layer 2 could read.
//!
//! `OverlayPermission`, `DirectorySpec` and `ContextScope` live here rather
//! than in the engine because the parser constructs them and a Layer 0 parser
//! cannot name a Layer 1 type. `engine::container::options` and
//! `engine::overlay` re-export all three for one release, so existing paths
//! still compile.

use crate::data::config::EffectiveConfig;
use crate::data::error::DataError;

/// Read-only or read-write, as a mount is granted to an agent.
///
/// Moved here from `engine::container::options` by WI 0114 F-27: it is the
/// permission half of every parsed overlay spec, so the Layer 0 grammar has
/// to be able to name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorySpec {
    pub host: String,
    pub container: String,
    pub permission: OverlayPermission,
}

/// Scope for a context overlay. Moved here from `engine::overlay` by WI 0114
/// F-27 so the Layer 0 grammar can construct it; the engine re-exports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextScope {
    Global,
    Repo,
    Workflow,
}

/// Specification for a skill overlay: all skills or a named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSpec {
    All,
    Named(String),
}

/// A parsed context overlay specification (scope + permission).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOverlaySpec {
    pub scope: ContextScope,
    pub permission: OverlayPermission,
}

/// A parsed overlay expression: directory mount, skill, env passthrough, or context.
#[derive(Debug, Clone, PartialEq)]
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

        // 1. Global config overlays (lowest priority).
        if let Some(overlay_strs) = ec.global().overlays.as_ref() {
            for s in overlay_strs {
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

        // 2. Repo config overlays.
        if let Some(overlay_strs) = ec.repo().overlays.as_ref() {
            for s in overlay_strs {
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

        // 3. AWMAN_OVERLAYS env var.
        if let Some(env_str) = ec.env().overlays() {
            let parsed =
                parse_overlay_list(env_str).map_err(|reason| DataError::InvalidOverlaySpec {
                    spec: format!("AWMAN_OVERLAYS: {env_str}"),
                    reason,
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

#[cfg(test)]
mod env_overlay_parser_tests {
    use super::*;

    #[test]
    fn env_accepts_an_ordinary_environment_variable_name() {
        for name in ["GITHUB_TOKEN", "_private", "AWS_PROFILE2", "X"] {
            assert_eq!(
                parse_overlay_list(&format!("env({name})")).unwrap(),
                vec![TypedOverlay::Env(name.to_string())],
                "{name} is a perfectly ordinary variable name"
            );
        }
    }

    /// Remediation of review-security F11. The argument becomes an environment
    /// variable *name*: `-e NAME` in argv and, for a squad daemon, a
    /// `Command::env` key on the spawned container CLI. A `=` there makes a
    /// malformed entry instead of a passthrough and a NUL fails the spawn, so a
    /// bad name is refused when the task is created rather than discovered at
    /// its first scheduled run.
    #[test]
    fn env_rejects_a_name_that_is_not_an_environment_variable_name() {
        for bad in [
            "FOO=bar",
            "2FOO",
            "FOO BAR",
            "FOO-BAR",
            "FOO\u{0}BAR",
            "FOO.BAR",
        ] {
            let err = parse_overlay_list(&format!("env({bad})")).unwrap_err_or_else_name(bad);
            assert!(
                err.contains("not a valid environment variable name"),
                "env({bad}) must be refused by name, not by accident; got: {err}"
            );
        }
    }

    /// A helper that reports the offending input when the parse unexpectedly
    /// succeeds, since a silent `unwrap_err` panic names nothing.
    trait UnwrapErrNamed {
        fn unwrap_err_or_else_name(self, input: &str) -> String;
    }
    impl UnwrapErrNamed for Result<Vec<TypedOverlay>, String> {
        fn unwrap_err_or_else_name(self, input: &str) -> String {
            match self {
                Ok(parsed) => panic!("env({input}) must not parse; got {parsed:?}"),
                Err(e) => e,
            }
        }
    }
}
#[cfg(test)]
mod skill_parser_tests {
    use super::*;

    #[test]
    fn skill_empty_returns_error() {
        let err = parse_overlay_list("skill()").unwrap_err();
        assert!(
            err.contains("requires an argument"),
            "error must mention 'requires an argument'; got: {err}"
        );
    }

    #[test]
    fn skill_star_parses_to_skill_all() {
        let result = parse_overlay_list("skill(*)").unwrap();
        assert_eq!(result, vec![TypedOverlay::Skill(SkillSpec::All)]);
    }

    #[test]
    fn skill_named_parses_to_skill_named() {
        let result = parse_overlay_list("skill(myskill)").unwrap();
        assert_eq!(
            result,
            vec![TypedOverlay::Skill(SkillSpec::Named("myskill".to_string()))]
        );
    }

    #[test]
    fn skill_library_slash_skill_parses_to_named() {
        // `skill(library/skill)` (exactly one slash) is a valid single-skill
        // reference into a pulled library — it parses as a Named spec.
        let result = parse_overlay_list("skill(superpowers/brainstorming)").unwrap();
        assert_eq!(
            result,
            vec![TypedOverlay::Skill(SkillSpec::Named(
                "superpowers/brainstorming".to_string()
            ))]
        );
    }

    #[test]
    fn skill_with_two_slashes_is_rejected() {
        // More than one slash is ambiguous (`library/skill` is the deepest
        // form) and must be rejected at parse time with a descriptive error.
        let err = parse_overlay_list("skill(a/b/c)").unwrap_err();
        assert!(
            err.contains("at most one '/'") && err.contains("a/b/c"),
            "error must explain the one-slash limit and echo the bad value; got: {err}"
        );
    }

    /// Segments are joined onto host skills paths at mount time, so a `.`,
    /// `..` or empty segment would resolve somewhere the reference never named
    /// — `skill(superpowers/..)` would mount the whole managed clone, `.git/`
    /// included (WI-0103 remediation).
    #[test]
    fn skill_with_traversal_or_empty_segments_is_rejected() {
        for bad in [
            "superpowers/..",
            "superpowers/.",
            "../superpowers",
            "./superpowers",
            "superpowers/",
            "/superpowers",
            "..",
            ".",
        ] {
            let expr = format!("skill({bad})");
            let err = parse_overlay_list(&expr)
                .err()
                .unwrap_or_else(|| panic!("'{expr}' must be rejected at parse time"));
            assert!(
                err.contains("must not be empty, '.' or '..'"),
                "'{expr}' must be rejected with the segment rule; got: {err}"
            );
        }
    }

    /// The new segment rule must not narrow what already parsed.
    #[test]
    fn skill_ordinary_names_still_parse_after_segment_validation() {
        for good in [
            "lint",
            "superpowers",
            "superpowers/brainstorming",
            "my.skill",
        ] {
            let expr = format!("skill({good})");
            let parsed = parse_overlay_list(&expr)
                .unwrap_or_else(|e| panic!("'{expr}' must still parse; got error: {e}"));
            assert_eq!(
                parsed,
                vec![TypedOverlay::Skill(SkillSpec::Named(good.to_string()))],
                "'{expr}' must parse to the same named skill as before"
            );
        }
    }

    #[test]
    fn skill_and_dir_in_comma_list_produces_both_variants() {
        let result = parse_overlay_list("skill(*),dir(/host:/container:ro)").unwrap();
        assert_eq!(result.len(), 2, "expected 2 overlays; got {result:?}");
        assert!(
            matches!(result[0], TypedOverlay::Skill(SkillSpec::All)),
            "first entry must be Skill(All); got {result:?}"
        );
        assert!(
            matches!(result[1], TypedOverlay::Directory(_)),
            "second entry must be Directory; got {result:?}"
        );
    }

    #[test]
    fn unknown_tag_error_lists_supported_types() {
        let err = parse_overlay_list("foobar(/x:/y)").unwrap_err();
        assert!(
            err.contains("dir"),
            "error must mention 'dir' as a supported type; got: {err}"
        );
        assert!(
            err.contains("skill"),
            "error must mention 'skill' as a supported type; got: {err}"
        );
        assert!(
            err.contains("ssh"),
            "error must mention 'ssh' as a supported type; got: {err}"
        );
        assert!(
            err.contains("env"),
            "error must mention 'env' as a supported type; got: {err}"
        );
    }

    #[test]
    fn ssh_parses_to_directory_overlay() {
        let result = parse_overlay_list("ssh()").unwrap();
        assert_eq!(result.len(), 1);
        match &result[0] {
            TypedOverlay::Directory(spec) => {
                assert!(
                    spec.host.ends_with(".ssh"),
                    "host must end with .ssh; got: {}",
                    spec.host
                );
                assert_eq!(spec.container, "~/.ssh");
            }
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn env_parses_to_env_variant() {
        let result = parse_overlay_list("env(MY_VAR)").unwrap();
        assert_eq!(result, vec![TypedOverlay::Env("MY_VAR".to_string())]);
    }

    #[test]
    fn env_empty_returns_error() {
        let err = parse_overlay_list("env()").unwrap_err();
        assert!(
            err.contains("requires an argument"),
            "error must mention 'requires an argument'; got: {err}"
        );
    }

    #[test]
    fn ssh_with_arguments_returns_error() {
        let err = parse_overlay_list("ssh(foo)").unwrap_err();
        assert!(
            err.contains("ssh()") || err.contains("no arguments"),
            "error must explain ssh() takes no arguments; got: {err}"
        );
    }

    #[test]
    fn ssh_overlay_has_read_only_permission() {
        let result = parse_overlay_list("ssh()").unwrap();
        match &result[0] {
            TypedOverlay::Directory(spec) => {
                assert_eq!(
                    spec.permission,
                    OverlayPermission::ReadOnly,
                    "ssh() must produce a ReadOnly mount; got: {:?}",
                    spec.permission
                );
                assert_eq!(spec.container, "~/.ssh", "container path must be ~/.ssh");
            }
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn skill_multiple_args_returns_error() {
        let err = parse_overlay_list("skill(foo, bar)").unwrap_err();
        assert!(
            err.contains("separate skill()") || err.contains("one argument"),
            "error must direct user to separate skill() calls; got: {err}"
        );
    }

    #[test]
    fn skills_plural_named_returns_error_with_migration_hint() {
        let err = parse_overlay_list("skills(foo)").unwrap_err();
        assert!(
            err.contains("removed") || err.contains("skill("),
            "error must mention the removed form and replacement; got: {err}"
        );
    }

    #[test]
    fn skills_plural_star_returns_error_with_migration_hint() {
        let err = parse_overlay_list("skills(*)").unwrap_err();
        assert!(
            err.contains("skill(*)") || err.contains("removed"),
            "error must mention skill(*) replacement; got: {err}"
        );
    }

    #[test]
    fn skills_plural_empty_returns_error_with_migration_hint() {
        let err = parse_overlay_list("skills()").unwrap_err();
        assert!(
            err.contains("skill(*)") || err.contains("removed"),
            "error must mention skill(*) or removed form; got: {err}"
        );
    }

    #[test]
    fn env_multiple_args_returns_error() {
        let err = parse_overlay_list("env(A, B)").unwrap_err();
        assert!(
            err.contains("separate env()") || err.contains("one argument"),
            "error must direct user to use separate env() calls; got: {err}"
        );
    }

    #[test]
    fn env_list_produces_two_separate_env_overlays() {
        let result = parse_overlay_list("env(A), env(B)").unwrap();
        assert_eq!(
            result.len(),
            2,
            "two env() expressions must produce two overlays; got {result:?}"
        );
        assert_eq!(result[0], TypedOverlay::Env("A".to_string()));
        assert_eq!(result[1], TypedOverlay::Env("B".to_string()));
    }
}
#[cfg(test)]
mod collect_overlay_specs_tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME, AWMAN_OVERLAYS};
    use crate::data::config::global::GlobalConfig;
    use crate::data::config::repo::RepoConfig;
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
    fn skills_enabled_when_repo_config_has_skill_star() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["skill(*)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, None)
            .unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled from repo config"
        );
    }

    #[test]
    fn skills_enabled_when_global_config_has_skill_star() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["skill(*)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        let session = open_session(git_tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, None)
            .unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled from global config"
        );
    }

    #[test]
    fn skills_enabled_when_awman_overlays_env_contains_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "skill(*)"),
        ]);
        let session = open_session(tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, None)
            .unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled when AWMAN_OVERLAYS contains skill(*)"
        );
    }

    #[test]
    fn skills_enabled_when_cli_typed_overlays_contains_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![TypedOverlay::Skill(SkillSpec::All)], None, None)
            .unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled from CLI TypedOverlay::Skill(All)"
        );
    }

    #[test]
    fn skills_disabled_when_no_source_enables_it() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, None)
            .unwrap();
        assert!(
            !collected.include_all_skills,
            "skills must be disabled when no source sets it"
        );
    }

    #[test]
    fn skills_enabled_is_additive_or_single_source_sufficient() {
        // Only global config has skill(*); repo config and CLI do not.
        // include_all_skills must still be true — OR semantics, not AND.
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["skill(*)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        // Repo config has no overlays; no CLI TypedOverlay::Skill.
        let session = open_session(git_tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, None)
            .unwrap();
        assert!(
            collected.include_all_skills,
            "a single source (global config) must be sufficient to enable skills (additive OR)"
        );
    }

    #[test]
    fn env_passthrough_collected_from_overlay_expressions() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "env(GH_TOKEN),env(AWS_PROFILE)"),
        ]);
        let session = open_session(tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, None)
            .unwrap();
        assert_eq!(collected.env_passthrough, vec!["GH_TOKEN", "AWS_PROFILE"]);
    }

    // ─── New tests for WI-0082 ────────────────────────────────────────────────

    #[test]
    fn malformed_awman_overlays_env_var_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "###not-an-overlay###"),
        ]);
        let session = open_session(tmp.path(), env);

        let result = session
            .effective_config()
            .collected_overlays(vec![], None, None);
        assert!(result.is_err(), "malformed AWMAN_OVERLAYS must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("###not-an-overlay###") || msg.contains("invalid overlay"),
            "error must identify the bad spec; got: {msg}"
        );
    }

    #[test]
    fn skill_star_in_flag_and_named_in_step_union_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let cli_overlays = vec![TypedOverlay::Skill(SkillSpec::All)];
        let step_overlays = vec!["skill(foo)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(cli_overlays, None, Some(&step_overlays))
            .unwrap();

        assert!(
            collected.include_all_skills,
            "skill(*) in CLI flags must set include_all_skills to true"
        );
        assert!(
            collected.named_skills.contains(&"foo".to_string()),
            "skill(foo) from step must accumulate in named_skills; got {:?}",
            collected.named_skills
        );
    }

    #[test]
    fn skill_named_in_repo_and_step_both_accumulate() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["skill(foo)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["skill(bar)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, Some(&step_overlays))
            .unwrap();

        assert!(
            !collected.include_all_skills,
            "no skill(*) source; include_all_skills must be false"
        );
        assert!(
            collected.named_skills.contains(&"foo".to_string()),
            "skill(foo) from repo config must be in named_skills; got {:?}",
            collected.named_skills
        );
        assert!(
            collected.named_skills.contains(&"bar".to_string()),
            "skill(bar) from step must be in named_skills; got {:?}",
            collected.named_skills
        );
    }

    #[test]
    fn skills_plural_in_repo_config_returns_err_with_migration_message() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["skills()".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let result = session
            .effective_config()
            .collected_overlays(vec![], None, None);
        assert!(result.is_err(), "skills() in repo config must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("skill(") || msg.contains("removed"),
            "error must contain migration guidance; got: {msg}"
        );
    }

    #[test]
    fn skills_plural_named_in_env_var_returns_err_with_migration_message() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "skills(foo)"),
        ]);
        let session = open_session(tmp.path(), env);

        let result = session
            .effective_config()
            .collected_overlays(vec![], None, None);
        assert!(
            result.is_err(),
            "skills(foo) in AWMAN_OVERLAYS must return Err"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("skill(") || msg.contains("removed"),
            "error must contain migration guidance; got: {msg}"
        );
    }

    #[test]
    fn ssh_from_flag_and_step_both_resolve_to_same_host_path() {
        // ssh() from any source expands to the same ~/.ssh host path.
        // After passing through OverlayEngine::build_overlays, there must be
        // exactly one mount for that path (insert_or_merge deduplication).
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let ssh_typed = parse_overlay_list("ssh()").unwrap().remove(0);
        let step_overlays = vec!["ssh()".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![ssh_typed], None, Some(&step_overlays))
            .unwrap();

        let ssh_entries: Vec<_> = collected
            .directories
            .iter()
            .filter(|d| d.host.ends_with(".ssh"))
            .collect();
        assert!(
            !ssh_entries.is_empty(),
            "at least one ssh() overlay must appear in directories"
        );
        let unique_hosts: std::collections::HashSet<_> =
            ssh_entries.iter().map(|d| d.host.as_str()).collect();
        assert_eq!(
            unique_hosts.len(),
            1,
            "all ssh() expansions from any source must resolve to the same host path; \
             got unique hosts: {unique_hosts:?}"
        );
    }

    #[test]
    fn env_from_two_sources_deduplicates_to_one_entry() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["env(MY_TOKEN)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["env(MY_TOKEN)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, Some(&step_overlays))
            .unwrap();

        let count = collected
            .env_passthrough
            .iter()
            .filter(|v| *v == "MY_TOKEN")
            .count();
        assert_eq!(
            count, 1,
            "MY_TOKEN from two sources must appear exactly once in env_passthrough; \
             got {:?}",
            collected.env_passthrough
        );
    }

    #[test]
    fn env_from_repo_config_and_step_both_present_in_passthrough() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["env(REPO_VAR)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["env(STEP_VAR)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, Some(&step_overlays))
            .unwrap();

        assert!(
            collected.env_passthrough.contains(&"REPO_VAR".to_string()),
            "env(REPO_VAR) from repo config must be in env_passthrough; got {:?}",
            collected.env_passthrough
        );
        assert!(
            collected.env_passthrough.contains(&"STEP_VAR".to_string()),
            "env(STEP_VAR) from step overlays must be in env_passthrough; got {:?}",
            collected.env_passthrough
        );
    }
}
#[cfg(test)]
mod overlay_spec_tests {
    use super::*;

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
// ─── WI-0087: context() overlay parsing ──────────────────────────────────────

#[cfg(test)]
mod context_parser_tests {
    use super::*;

    fn parse_context(input: &str) -> Result<(ContextScope, OverlayPermission), String> {
        let result = parse_overlay_list(input)?;
        assert_eq!(
            result.len(),
            1,
            "expected exactly 1 overlay; got {result:?}"
        );
        match &result[0] {
            TypedOverlay::Context(spec) => Ok((spec.scope, spec.permission)),
            other => Err(format!("expected TypedOverlay::Context, got {other:?}")),
        }
    }

    #[test]
    fn context_global_default_rw() {
        let (scope, perm) = parse_context("context(global)").unwrap();
        assert_eq!(scope, ContextScope::Global);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_repo_default_rw() {
        let (scope, perm) = parse_context("context(repo)").unwrap();
        assert_eq!(scope, ContextScope::Repo);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_workflow_default_rw() {
        let (scope, perm) = parse_context("context(workflow)").unwrap();
        assert_eq!(scope, ContextScope::Workflow);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_global_ro_explicit() {
        let (scope, perm) = parse_context("context(global:ro)").unwrap();
        assert_eq!(scope, ContextScope::Global);
        assert_eq!(perm, OverlayPermission::ReadOnly);
    }

    #[test]
    fn context_workflow_rw_explicit() {
        let (scope, perm) = parse_context("context(workflow:rw)").unwrap();
        assert_eq!(scope, ContextScope::Workflow);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_repo_ro() {
        let (scope, perm) = parse_context("context(repo:ro)").unwrap();
        assert_eq!(scope, ContextScope::Repo);
        assert_eq!(perm, OverlayPermission::ReadOnly);
    }

    // ─── Error cases ──────────────────────────────────────────────────────────

    #[test]
    fn context_missing_scope_returns_error() {
        let err = parse_overlay_list("context()").unwrap_err();
        assert!(
            err.contains("scope") || err.contains("requires"),
            "error must explain context() requires a scope; got: {err}"
        );
    }

    #[test]
    fn context_unknown_scope_returns_error() {
        let err = parse_overlay_list("context(unknown)").unwrap_err();
        assert!(
            err.contains("unknown"),
            "error must identify the unknown scope value; got: {err}"
        );
        // Must mention the supported scopes.
        assert!(
            err.contains("global") || err.contains("scope"),
            "error must name supported scopes; got: {err}"
        );
    }

    #[test]
    fn context_bad_permission_returns_error() {
        // context(global:rx) — 'rx' is not a valid permission
        let err = parse_overlay_list("context(global:rx)").unwrap_err();
        assert!(
            err.contains("rx") || err.contains("permission"),
            "error must mention the bad permission 'rx'; got: {err}"
        );
    }

    #[test]
    fn context_too_many_parts_returns_error() {
        // context(global:ro:extra) — the ':'-split yields "ro:extra" as perm
        let err = parse_overlay_list("context(global:ro:extra)").unwrap_err();
        assert!(
            err.contains("ro:extra") || err.contains("permission"),
            "error must identify the malformed permission segment; got: {err}"
        );
    }
}
// ─── WI-0087: EffectiveConfig::collected_overlays with context overlays ─────────────────

#[cfg(test)]
mod context_collect_0087_tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::config::global::GlobalConfig;
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
    fn context_global_from_global_config_appears_in_context_overlays() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["context(global)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        let session = open_session(git_tmp.path(), env);

        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, None)
            .unwrap();
        assert_eq!(
            collected.context_overlays.len(),
            1,
            "one context overlay expected; got {:?}",
            collected.context_overlays
        );
        assert_eq!(collected.context_overlays[0].scope, ContextScope::Global);
    }

    #[test]
    fn context_global_deduplicates_when_in_global_config_and_step() {
        // context(global) in both global config and step overlays → exactly one entry.
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["context(global)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["context(global)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![], None, Some(&step_overlays))
            .unwrap();

        let global_count = collected
            .context_overlays
            .iter()
            .filter(|c| c.scope == ContextScope::Global)
            .count();
        assert_eq!(
            global_count, 1,
            "context(global) from two sources must deduplicate to one entry; \
             got {:?}",
            collected.context_overlays
        );
    }

    #[test]
    fn workflow_level_context_global_applies_to_step() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let workflow_overlays = vec!["context(global)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![], Some(&workflow_overlays), None)
            .unwrap();

        assert_eq!(
            collected.context_overlays.len(),
            1,
            "workflow-level context(global) must appear in context_overlays; \
             got {:?}",
            collected.context_overlays
        );
        assert_eq!(collected.context_overlays[0].scope, ContextScope::Global);
    }

    #[test]
    fn workflow_and_step_context_union_produces_two_entries() {
        // Workflow-level context(repo) + step-level context(global) → two entries.
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let workflow_overlays = vec!["context(repo)".to_string()];
        let step_overlays = vec!["context(global)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![], Some(&workflow_overlays), Some(&step_overlays))
            .unwrap();

        assert_eq!(
            collected.context_overlays.len(),
            2,
            "workflow-level context(repo) + step-level context(global) must \
             produce two distinct context overlay specs; got {:?}",
            collected.context_overlays
        );
        let has_repo = collected
            .context_overlays
            .iter()
            .any(|c| c.scope == ContextScope::Repo);
        let has_global = collected
            .context_overlays
            .iter()
            .any(|c| c.scope == ContextScope::Global);
        assert!(has_repo, "Repo scope must be present");
        assert!(has_global, "Global scope must be present");
    }

    #[test]
    fn step_context_workflow_does_not_override_workflow_level() {
        // Step-level context(workflow) + workflow-level context(global) → two entries (union).
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let workflow_overlays = vec!["context(global)".to_string()];
        let step_overlays = vec!["context(workflow)".to_string()];
        let collected = session
            .effective_config()
            .collected_overlays(vec![], Some(&workflow_overlays), Some(&step_overlays))
            .unwrap();

        assert_eq!(
            collected.context_overlays.len(),
            2,
            "union semantics: both workflow-level and step-level context scopes \
             must be present; got {:?}",
            collected.context_overlays
        );
        assert!(
            collected
                .context_overlays
                .iter()
                .any(|c| c.scope == ContextScope::Global),
            "Global scope from workflow level must be present"
        );
        assert!(
            collected
                .context_overlays
                .iter()
                .any(|c| c.scope == ContextScope::Workflow),
            "Workflow scope from step level must be present"
        );
    }
}
