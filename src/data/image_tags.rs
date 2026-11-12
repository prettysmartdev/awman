//! Image tag and repo-hash helpers — Layer 0.
//!
//! Pure functions used by `AgentEngine` and `ContainerRuntime` to derive
//! deterministic image tags from a git-root path. Layer 0 owns this so both
//! engines can share the same algorithm without one calling the other.

use std::path::Path;

use crate::data::fs::hash::sha256_hex;

/// 8-hex-char SHA-256 prefix of the canonicalized git-root path. Used as a
/// stable identifier for per-repo image tags and per-repo state filenames.
pub fn repo_hash(git_root: &Path) -> String {
    let canon = std::fs::canonicalize(git_root)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| git_root.to_string_lossy().to_string());
    sha256_hex(&canon).chars().take(8).collect()
}

/// Project (base) image tag: `awman-<stem>:latest` where `<stem>` is the
/// repo folder name, or `squad-<task-slug>` for a squad task workspace (see
/// [`image_stem`]).
///
/// Falls back to `awman-repo:latest` when the git-root has no file_name() (root `/`).
pub fn project_image_tag(git_root: &Path) -> String {
    format!("awman-{}:latest", image_stem(git_root))
}

/// Per-agent image tag: `awman-<stem>-<agent>:latest` (same stem rules as
/// [`project_image_tag`]).
pub fn agent_image_tag(git_root: &Path, agent: &str) -> String {
    format!("awman-{}-{agent}:latest", image_stem(git_root))
}

/// The naming stem images for `git_root` are tagged with.
///
/// Ordinarily the root's folder name. A squad default task workspace is the
/// exception: every task's durable workspace is a directory literally named
/// `workspace` (`…/tasks/<slug>/workspace`), so folder-derived tags would
/// collapse every task onto one `awman-workspace:latest` family and the
/// tasks would clobber each other's images. Those roots use
/// `squad-<task-slug>` instead, so the task's unique slug is part of every
/// image name squad builds.
fn image_stem(git_root: &Path) -> String {
    if let Some(slug) = squad_task_slug(git_root) {
        return format!("squad-{slug}");
    }
    let folder = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let stem = sanitize_image_name(folder);
    if stem.is_empty() {
        "repo".to_string()
    } else {
        stem
    }
}

/// Coerce a folder name into a valid OCI repository name component
/// (`[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`). Docker and Apple containers both
/// reject anything else — e.g. `MeshCore` must become `meshcore`.
///
/// Uppercase is lowered; any other disallowed character becomes `-`. A run
/// of separators is kept when it is already valid (`.`, `_`, `__`, or only
/// hyphens) and collapsed to a single `-` otherwise. Leading and trailing
/// separators are dropped, so the result may be empty.
fn sanitize_image_name(name: &str) -> String {
    let is_sep = |c: char| matches!(c, '.' | '_' | '-');
    let mapped: String = name
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || is_sep(c) {
                c
            } else {
                '-'
            }
        })
        .collect();

    let mut out = String::with_capacity(mapped.len());
    let mut sep_run = String::new();
    for c in mapped.chars() {
        if is_sep(c) {
            sep_run.push(c);
            continue;
        }
        if !sep_run.is_empty() && !out.is_empty() {
            let valid =
                matches!(sep_run.as_str(), "." | "_" | "__") || sep_run.chars().all(|s| s == '-');
            out.push_str(if valid { &sep_run } else { "-" });
        }
        sep_run.clear();
        out.push(c);
    }
    out
}

/// The task slug when `git_root` is a squad task workspace — a path ending in
/// `tasks/<slug>/workspace` whose `<slug>` is a valid task slug (lowercase
/// alphanumerics and interior hyphens, the charset task creation enforces).
/// Structural, not env-dependent: the squad root itself is relocatable via
/// `AWMAN_SQUAD_ROOT`, so only the fixed trailing layout identifies it.
fn squad_task_slug(git_root: &Path) -> Option<String> {
    let mut components = git_root.components().rev();
    let leaf = components.next()?.as_os_str().to_str()?;
    let slug = components.next()?.as_os_str().to_str()?;
    let tasks = components.next()?.as_os_str().to_str()?;
    if leaf != "workspace" || tasks != "tasks" {
        return None;
    }
    let is_lower_alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let valid = !slug.is_empty()
        && slug.chars().all(|c| is_lower_alnum(c) || c == '-')
        && is_lower_alnum(slug.chars().next()?)
        && is_lower_alnum(slug.chars().next_back()?);
    valid.then(|| slug.to_string())
}
