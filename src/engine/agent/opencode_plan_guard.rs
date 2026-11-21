//! OpenCode Plan-profile protection for `--plan` (WI 0070) — Layer 1.
//!
//! awman's `--plan` for OpenCode selects the provider's *built-in* Plan
//! agent (`--agent plan` on 1.x and v2 headless, `default_agent: "plan"` on
//! the v2 full TUI). User or project configuration can redefine or disable
//! that agent, or (v2) name another default that outranks awman's private
//! patch. A launch that cannot prove it gets the built-in Plan agent is
//! refused with `AgentModeConflict`; it never falls back to Build.
//!
//! [`collect_opencode_sources`] does the bounded, no-follow reads;
//! [`check_opencode_plan`] is the pure rule set over what it read.

use std::path::{Path, PathBuf};

use super::definition::PlanConfigGuard;
use crate::engine::error::EngineError;

/// Largest configuration file the guard reads.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Keys an `agent.plan` / `mode.plan` override may set without changing
/// what the built-in Plan agent is allowed to do.
pub const ALLOWED_PLAN_KEYS: &[&str] = &[
    "model",
    "temperature",
    "top_p",
    "color",
    "description",
    "variant",
];

/// Environment names that relocate OpenCode's configuration where awman
/// cannot read it. `XDG_CONFIG_HOME` moves the global directory (and with it
/// the v2 private patch) to a location awman did not inspect.
pub const RELOCATING_ENV: &[&str] = &["OPENCODE_CONFIG", "OPENCODE_CONFIG_DIR", "XDG_CONFIG_HOME"];

/// Configuration file names OpenCode reads in each configuration directory.
pub const CONFIG_FILE_NAMES: &[&str] = &["opencode.json", "opencode.jsonc"];
/// Agent-definition subdirectories OpenCode reads in each configuration
/// directory; `<sub>/plan.md` redefines the Plan agent.
pub const AGENT_DIR_NAMES: &[&str] = &["agent", "agents"];

/// Every file, relative to a configuration directory (the global directory
/// or the repository's `.opencode/`), that the guard inspects there.
pub fn config_dir_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = CONFIG_FILE_NAMES.iter().map(PathBuf::from).collect();
    files.extend(
        AGENT_DIR_NAMES
            .iter()
            .map(|sub| Path::new(sub).join("plan.md")),
    );
    files
}

/// Every file, relative to the repository root, that OpenCode reads as
/// project configuration and the guard therefore inspects: the direct
/// JSON/JSONC files and everything under `.opencode/` (whose JSON outranks
/// the direct files). The mount check in `AgentEngine` uses the same list to
/// prove the guest is given exactly these host files.
pub fn project_config_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = CONFIG_FILE_NAMES.iter().map(PathBuf::from).collect();
    files.extend(
        config_dir_files()
            .into_iter()
            .map(|file| Path::new(".opencode").join(file)),
    );
    files
}
/// Environment name carrying inline configuration that outranks the files.
pub const INLINE_CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// What kind of source a configuration text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// `opencode.json` / `opencode.jsonc` / inline JSON.
    Json,
    /// An `agent(s)/plan.md` agent definition.
    MarkdownAgent,
}

/// Where a source sits in OpenCode's precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceScope {
    /// The execution-private copy awman seeded and is about to patch (v2).
    PrivateGlobal,
    /// The host global directory, direct-mounted (1.x).
    Global,
    /// The repository's own configuration.
    Project,
    /// `OPENCODE_CONFIG_CONTENT` passed to the guest.
    Environment,
}

/// One configuration text the guard checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeConfigSource {
    /// Shown in a refusal (`project opencode.json`).
    pub origin: String,
    pub kind: SourceKind,
    pub scope: SourceScope,
    pub text: String,
}

fn conflict(agent: &str, selector: String, reason: &str) -> EngineError {
    EngineError::AgentModeConflict {
        agent: agent.to_string(),
        selector,
        origin: "settings",
        reason: reason.to_string(),
    }
}

/// Check the sources for a `--plan` launch of an OpenCode definition.
/// Returns Info notices (a v2 private `default_agent` that the patch
/// overrides) or the first conflict. `settings_delivery` is true when the
/// launch selects Plan through `default_agent` (v2 full TUI). Pure.
pub fn check_opencode_plan(
    guard: PlanConfigGuard,
    agent: &str,
    sources: &[OpenCodeConfigSource],
    settings_delivery: bool,
) -> Result<Vec<String>, EngineError> {
    // `default_agent` matters only where awman selects Plan through it (the
    // v2 full TUI); an explicit `--agent plan` outranks it.
    let checks_default_agent = match guard {
        PlanConfigGuard::None => return Ok(Vec::new()),
        PlanConfigGuard::OpenCodeV1 => false,
        PlanConfigGuard::OpenCodeV2 => settings_delivery,
    };
    let mut notices = Vec::new();
    for source in sources {
        if source.kind == SourceKind::MarkdownAgent {
            return Err(conflict(
                agent,
                source.origin.clone(),
                "it redefines the built-in Plan agent; remove or rename it to use --plan",
            ));
        }
        let value = crate::data::jsonc::parse_tolerant(&source.text).map_err(|e| {
            conflict(
                agent,
                source.origin.clone(),
                &format!("awman cannot parse it ({e}), so it cannot verify the Plan agent"),
            )
        })?;
        let Some(root) = value.as_object() else {
            return Err(conflict(
                agent,
                source.origin.clone(),
                "its root is not a JSON object, so awman cannot verify the Plan agent",
            ));
        };
        // Schema by provider major: v2 configures agents under the plural
        // `agents` object; its singular and legacy `mode` tables stay
        // inspected because they are still accepted for compatibility.
        let tables: &[&str] = match guard {
            PlanConfigGuard::OpenCodeV2 => &["agent", "agents", "mode"],
            _ => &["agent", "mode"],
        };
        for &table in tables {
            let Some(plan) = root.get(table).and_then(|t| t.get("plan")) else {
                continue;
            };
            let Some(plan) = plan.as_object() else {
                return Err(conflict(
                    agent,
                    format!("{} {table}.plan", source.origin),
                    "it replaces the built-in Plan agent definition",
                ));
            };
            if let Some(key) = plan
                .keys()
                .find(|key| !ALLOWED_PLAN_KEYS.contains(&key.as_str()))
            {
                return Err(conflict(
                    agent,
                    format!("{} {table}.plan.{key}", source.origin),
                    "it changes or disables the built-in Plan agent (only model, temperature, \
                     top_p, color, description and variant may be overridden)",
                ));
            }
        }
        if checks_default_agent {
            if let Some(default) = root.get("default_agent") {
                if default.as_str() != Some("plan") {
                    let shown = default.to_string();
                    match source.scope {
                        SourceScope::PrivateGlobal | SourceScope::Global => notices.push(format!(
                            "overrode default_agent={shown} for this session; host \
                             configuration unchanged"
                        )),
                        SourceScope::Project | SourceScope::Environment => {
                            return Err(conflict(
                                agent,
                                format!("{} default_agent={shown}", source.origin),
                                "it outranks awman's private default_agent=plan; remove it to \
                                 use --plan on the full-screen interface, or run headless",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(notices)
}

/// Read one configuration file: regular files only, never through a
/// symlink, at most [`MAX_CONFIG_BYTES`]. `Ok(None)` when absent.
fn read_bounded(agent: &str, path: &Path, origin: &str) -> Result<Option<String>, EngineError> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let cannot = |why: &str| {
        conflict(
            agent,
            origin.to_string(),
            &format!("awman cannot verify it ({why})"),
        )
    };
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => return Ok(None),
        Err(_) => return Err(cannot("not a readable regular file")),
    };
    let meta = file.metadata().map_err(|_| cannot("unreadable"))?;
    if !meta.is_file() {
        return Err(cannot("not a regular file"));
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(cannot("larger than 1 MiB"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| cannot("unreadable"))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(cannot("larger than 1 MiB"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| cannot("not UTF-8"))
}

/// A configuration directory inside the workspace that is a symlink can
/// resolve to a different file in the guest than on the host, so it cannot
/// be verified.
fn reject_symlinked_dir(agent: &str, dir: &Path, origin: &str) -> Result<(), EngineError> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(conflict(
            agent,
            origin.to_string(),
            "awman cannot verify it (a symlinked configuration directory)",
        )),
        _ => Ok(()),
    }
}

/// Read each of `files` (relative to `dir`) that exists. In the workspace
/// (`check_dirs`) every intermediate directory must be a real directory.
fn push_sources(
    agent: &str,
    out: &mut Vec<OpenCodeConfigSource>,
    dir: &Path,
    files: &[PathBuf],
    label: &str,
    scope: SourceScope,
    check_dirs: bool,
) -> Result<(), EngineError> {
    for file in files {
        let origin = format!("{label} {}", file.display());
        if check_dirs {
            let mut parent = file.parent();
            while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                reject_symlinked_dir(agent, &dir.join(p), &format!("{label} {}", p.display()))?;
                parent = p.parent();
            }
        }
        if let Some(text) = read_bounded(agent, &dir.join(file), &origin)? {
            let kind = if file.extension().is_some_and(|e| e == "md") {
                SourceKind::MarkdownAgent
            } else {
                SourceKind::Json
            };
            out.push(OpenCodeConfigSource {
                origin,
                kind,
                scope,
                text,
            });
        }
    }
    Ok(())
}

/// The configuration a `--plan` launch must verify, in OpenCode's
/// precedence order (global, project, environment).
///
/// - `global_dir`: the host `~/.config/opencode` (1.x, direct-mounted) or
///   the execution-private seeded copy (v2).
/// - `project_root`: the mounted workspace root; its direct and `.opencode/`
///   configuration are both read ([`project_config_files`]).
/// - `guest_env`: every `(name, value)` the launch passes to the guest that
///   OpenCode reads; a relocating name is refused outright.
pub fn collect_opencode_sources(
    agent: &str,
    guard: PlanConfigGuard,
    global_dir: Option<&Path>,
    project_root: &Path,
    guest_env: &[(String, Option<String>)],
) -> Result<Vec<OpenCodeConfigSource>, EngineError> {
    if guard == PlanConfigGuard::None {
        return Ok(Vec::new());
    }
    for (name, _) in guest_env {
        if RELOCATING_ENV.contains(&name.as_str()) {
            return Err(EngineError::AgentModeConflict {
                agent: agent.to_string(),
                selector: name.clone(),
                origin: "environment",
                reason: "awman cannot verify a relocated OpenCode config with --plan; do not \
                         pass it to this launch"
                    .into(),
            });
        }
    }
    let mut sources = Vec::new();
    if let Some(dir) = global_dir {
        let (label, scope) = match guard {
            PlanConfigGuard::OpenCodeV2 => ("private global", SourceScope::PrivateGlobal),
            _ => ("global", SourceScope::Global),
        };
        push_sources(
            agent,
            &mut sources,
            dir,
            &config_dir_files(),
            label,
            scope,
            false,
        )?;
    }
    push_sources(
        agent,
        &mut sources,
        project_root,
        &project_config_files(),
        "project",
        SourceScope::Project,
        true,
    )?;
    for (name, value) in guest_env {
        if name == INLINE_CONFIG_ENV {
            let Some(text) = value else {
                return Err(EngineError::AgentModeConflict {
                    agent: agent.to_string(),
                    selector: name.clone(),
                    origin: "environment",
                    reason: "awman cannot read its value, so it cannot verify the Plan agent"
                        .into(),
                });
            };
            sources.push(OpenCodeConfigSource {
                origin: INLINE_CONFIG_ENV.into(),
                kind: SourceKind::Json,
                scope: SourceScope::Environment,
                text: text.clone(),
            });
        }
    }
    Ok(sources)
}
