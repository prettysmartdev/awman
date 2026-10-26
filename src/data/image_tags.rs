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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn project_image_tag_uses_folder_name() {
        let p = PathBuf::from("/tmp/myproj");
        assert_eq!(project_image_tag(&p), "awman-myproj:latest");
    }

    #[test]
    fn agent_image_tag_includes_agent() {
        let p = PathBuf::from("/tmp/myproj");
        assert_eq!(agent_image_tag(&p, "claude"), "awman-myproj-claude:latest");
    }

    /// Custom agent names (lowercase file stems) tag like any other agent.
    #[test]
    fn agent_image_tag_for_a_custom_agent_name() {
        let p = PathBuf::from("/tmp/myproj");
        assert_eq!(agent_image_tag(&p, "pi"), "awman-myproj-pi:latest");
        assert_eq!(
            agent_image_tag(&p, "my-agent_2"),
            "awman-myproj-my-agent_2:latest"
        );
    }

    #[test]
    fn squad_task_workspaces_are_tagged_by_task_slug_not_folder_name() {
        // Every default task workspace is a folder literally named
        // `workspace`; without the slug the tags would collide across tasks.
        let a = PathBuf::from("/home/u/.awman/squad/tasks/issue-triage/workspace");
        let b = PathBuf::from("/home/u/.awman/squad/tasks/nightly-sweep/workspace");
        assert_eq!(project_image_tag(&a), "awman-squad-issue-triage:latest");
        assert_eq!(project_image_tag(&b), "awman-squad-nightly-sweep:latest");
        assert_eq!(
            agent_image_tag(&a, "claude"),
            "awman-squad-issue-triage-claude:latest"
        );
        assert_ne!(project_image_tag(&a), project_image_tag(&b));
    }

    #[test]
    fn squad_detection_survives_a_relocated_squad_root() {
        // AWMAN_SQUAD_ROOT can move the root anywhere; only the trailing
        // `tasks/<slug>/workspace` layout identifies a task workspace.
        let p = PathBuf::from("/data/custom-root/tasks/deploy2/workspace");
        assert_eq!(project_image_tag(&p), "awman-squad-deploy2:latest");
    }

    #[test]
    fn ordinary_repos_keep_folder_derived_tags() {
        for p in [
            PathBuf::from("/tmp/workspace"),                  // no tasks/ parent
            PathBuf::from("/tmp/tasks/Upper_Case/workspace"), // invalid slug
            PathBuf::from("/r/tasks/foo/notworkspace"),       // wrong leaf
        ] {
            let tag = project_image_tag(&p);
            assert!(
                !tag.contains("squad-"),
                "{p:?} must not be treated as a squad workspace: {tag}"
            );
        }
    }

    #[test]
    fn image_tags_are_lowercased() {
        let p = PathBuf::from("/Users/me/MeshCore");
        assert_eq!(project_image_tag(&p), "awman-meshcore:latest");
        assert_eq!(
            agent_image_tag(&p, "claude"),
            "awman-meshcore-claude:latest"
        );
    }

    #[test]
    fn sanitize_image_name_produces_valid_components() {
        let cases = [
            ("myproj", "myproj"),
            ("MeshCore", "meshcore"),
            ("my_proj", "my_proj"),
            ("my__proj", "my__proj"),
            ("my.proj", "my.proj"),
            ("my--proj", "my--proj"),
            ("My Project", "my-project"),
            ("a._b", "a-b"),
            ("a___b", "a-b"),
            ("a..b", "a-b"),
            ("_.hidden-", "hidden"),
            ("café", "caf"),
            ("日本", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_image_name(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn unsanitizable_folder_falls_back_to_repo() {
        let p = PathBuf::from("/tmp/日本");
        assert_eq!(project_image_tag(&p), "awman-repo:latest");
    }

    #[test]
    fn repo_hash_is_eight_hex_chars() {
        let p = PathBuf::from("/nonexistent/path");
        let h = repo_hash(&p);
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
