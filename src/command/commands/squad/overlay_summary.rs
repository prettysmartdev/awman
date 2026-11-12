//! The overlay inventory rendered into the squad leader prompt (WI 0117).
//!
//! A squad leader designs a workflow it will never run itself, for containers
//! it will never see. Without this it has to guess whether `~/.ssh` is mounted
//! or `GITHUB_TOKEN` is set, and both guesses cost: guessing high produces
//! steps that die on a missing key, guessing low produces a workflow weaker
//! than the task was configured to support.
//!
//! # Data only
//!
//! This module produces five bullet lists, or `(none)`. Every heading and
//! sentence around them lives in `src/assets/dynamic/squad-leader-prompt.md`,
//! so the prompt reads and edits as prose in one place.
//!
//! # Why only the daemon can answer this
//!
//! A directory overlay with a missing host path fails fast before any container
//! starts, and an unresolvable named skill is reported before launch — those
//! kinds announce themselves. `env(VAR)` does not: a name the host has no value
//! for is *silently* absent. In a squad run that host is the daemon, whose
//! payload environment is pushed to it by clients rather than inherited from a
//! shell, so only the daemon knows which names resolve.
//!
//! Presence is therefore decided by [`host_var`], the same function
//! [`resolve_env_passthrough`] uses to build the `docker run` argv, so the
//! prompt and the container cannot disagree.
//!
//! # Names, never values
//!
//! Only names and their set/unset state are rendered. The prompt reaches an
//! agent and a run log, so it is held to the same names-only rule as the
//! daemon's env state.
//!
//! [`host_var`]: crate::data::config::env::host_var
//! [`resolve_env_passthrough`]: crate::engine::container::docker

use crate::command::commands::CollectedOverlays;
use crate::data::dynamic_workflow_assets::OverlayInventory;
use crate::engine::container::options::OverlayPermission;
use crate::engine::overlay::ContextScope;

/// Build the leader prompt's overlay inventory from `collected`, the task's
/// fully-merged overlay set.
pub fn overlay_inventory(collected: &CollectedOverlays) -> OverlayInventory {
    inventory(collected, &|name| {
        crate::data::config::env::host_var(name).is_some()
    })
}

/// The renderer proper, with environment lookup injected so tests can describe
/// a host without mutating the process environment.
fn inventory(collected: &CollectedOverlays, env_is_set: &dyn Fn(&str) -> bool) -> OverlayInventory {
    let (set, unset): (Vec<&String>, Vec<&String>) = collected
        .env_passthrough
        .iter()
        .partition(|name| env_is_set(name));

    OverlayInventory {
        directories: directories(collected),
        env_set: names(&set),
        env_unset: names(&unset),
        skills: skills(collected),
        context: context(collected),
    }
}

fn directories(collected: &CollectedOverlays) -> String {
    or_none(
        collected
            .directories
            .iter()
            .map(|dir| {
                format!(
                    "- `{}` ({}) — from host `{}`",
                    dir.container,
                    permission_word(dir.permission),
                    dir.host
                )
            })
            .collect(),
    )
}

fn skills(collected: &CollectedOverlays) -> String {
    let mut lines = Vec::new();
    if collected.include_all_skills {
        lines.push("- every hand-authored global skill".to_string());
    }
    lines.extend(
        collected
            .named_skills
            .iter()
            .map(|name| format!("- `{name}`")),
    );
    or_none(lines)
}

fn context(collected: &CollectedOverlays) -> String {
    // The workflow scope is retargeted to the durable task workspace for the
    // leader and every step alike, so the template reports it under the
    // always-present mounts. Listing it here too would read as a second,
    // separate directory.
    let lines = collected
        .context_overlays
        .iter()
        .filter(|spec| spec.scope != ContextScope::Workflow)
        .map(|spec| {
            let (name, path) = match spec.scope {
                ContextScope::Global => ("context(global)", "/awman/context/global"),
                ContextScope::Repo => ("context(repo)", "/awman/context/repo"),
                ContextScope::Workflow => unreachable!("filtered out above"),
            };
            format!(
                "- `{name}` → `{path}` ({})",
                permission_word(spec.permission)
            )
        })
        .collect();
    or_none(lines)
}

fn names(names: &[&String]) -> String {
    or_none(names.iter().map(|name| format!("- `{name}`")).collect())
}

/// A bullet list, or `(none)` when it is empty — never an empty string, so the
/// template's spacing stays static whatever the task carries.
fn or_none(lines: Vec<String>) -> String {
    if lines.is_empty() {
        return "(none)".to_string();
    }
    lines.join("\n")
}

fn permission_word(permission: OverlayPermission) -> &'static str {
    match permission {
        OverlayPermission::ReadOnly => "read-only",
        OverlayPermission::ReadWrite => "read-write",
    }
}
