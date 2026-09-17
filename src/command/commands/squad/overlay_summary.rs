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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::ContextOverlaySpec;
    use crate::engine::overlay::DirectorySpec;

    fn dir(host: &str, container: &str, permission: OverlayPermission) -> DirectorySpec {
        DirectorySpec {
            host: host.to_string(),
            container: container.to_string(),
            permission,
        }
    }

    fn nothing_set(_: &str) -> bool {
        false
    }

    fn everything_set(_: &str) -> bool {
        true
    }

    #[test]
    fn empty_overlays_state_every_absence_explicitly() {
        let out = inventory(&CollectedOverlays::default(), &nothing_set);
        for (field, name) in [
            (&out.directories, "directories"),
            (&out.env_set, "env_set"),
            (&out.env_unset, "env_unset"),
            (&out.skills, "skills"),
            (&out.context, "context"),
        ] {
            assert_eq!(
                field, "(none)",
                "{name} must state its absence, never render empty"
            );
        }
    }

    /// The template owns every heading and every sentence; a list that leaked
    /// prose back into the code would put the prompt's wording in two places.
    #[test]
    fn the_inventory_carries_no_headings_or_prose() {
        let collected = CollectedOverlays {
            directories: vec![dir("~/.ssh", "/root/.ssh", OverlayPermission::ReadOnly)],
            env_passthrough: vec!["GITHUB_TOKEN".into()],
            named_skills: vec!["lint".into()],
            context_overlays: vec![ContextOverlaySpec {
                scope: ContextScope::Global,
                permission: OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        };
        let out = inventory(&collected, &everything_set);
        for field in [
            &out.directories,
            &out.env_set,
            &out.env_unset,
            &out.skills,
            &out.context,
        ] {
            assert!(
                !field.contains('#'),
                "a list must carry no heading; the template owns those, got: {field}"
            );
            for line in field.lines() {
                assert!(
                    line.starts_with("- ") || line.starts_with('('),
                    "every line must be a bullet or a (none) statement, got: {line}"
                );
            }
        }
    }

    #[test]
    fn directories_render_container_path_permission_and_host_source() {
        let collected = CollectedOverlays {
            directories: vec![
                dir("~/.ssh", "/root/.ssh", OverlayPermission::ReadOnly),
                dir("/var/data", "/mnt/data", OverlayPermission::ReadWrite),
            ],
            ..Default::default()
        };
        let out = inventory(&collected, &nothing_set);
        assert_eq!(
            out.directories,
            "- `/root/.ssh` (read-only) — from host `~/.ssh`\n\
             - `/mnt/data` (read-write) — from host `/var/data`"
        );
    }

    #[test]
    fn env_vars_are_split_into_set_and_unset() {
        let collected = CollectedOverlays {
            env_passthrough: vec!["GITHUB_TOKEN".into(), "AWS_PROFILE".into()],
            ..Default::default()
        };
        let out = inventory(&collected, &|name| name == "GITHUB_TOKEN");
        assert_eq!(out.env_set, "- `GITHUB_TOKEN`");
        assert_eq!(out.env_unset, "- `AWS_PROFILE`");
    }

    #[test]
    fn all_env_vars_unset_still_names_them_as_missing() {
        let collected = CollectedOverlays {
            env_passthrough: vec!["GITHUB_TOKEN".into()],
            ..Default::default()
        };
        let out = inventory(&collected, &nothing_set);
        assert_eq!(out.env_set, "(none)");
        assert_eq!(out.env_unset, "- `GITHUB_TOKEN`");
    }

    #[test]
    fn env_values_never_appear_in_the_inventory() {
        // The lookup claims every name resolves; the renderer must still print
        // nothing but names — it is never handed a value and must never ask.
        // The prompt is echoed into a run log, so a `NAME=VALUE` pair here
        // would be a secret written to disk.
        let collected = CollectedOverlays {
            env_passthrough: vec!["GITHUB_TOKEN".into(), "AWS_PROFILE".into()],
            ..Default::default()
        };
        let out = inventory(&collected, &everything_set);
        for name in &collected.env_passthrough {
            assert!(
                !out.env_set.contains(&format!("{name}=")),
                "the inventory must never render a NAME=VALUE pair, got: {}",
                out.env_set
            );
        }
    }

    #[test]
    fn skills_render_as_wildcard_or_names() {
        let all = CollectedOverlays {
            include_all_skills: true,
            ..Default::default()
        };
        assert_eq!(
            inventory(&all, &nothing_set).skills,
            "- every hand-authored global skill"
        );

        let named = CollectedOverlays {
            named_skills: vec!["lint".into(), "superpowers/brainstorming".into()],
            ..Default::default()
        };
        assert_eq!(
            inventory(&named, &nothing_set).skills,
            "- `lint`\n- `superpowers/brainstorming`"
        );
    }

    #[test]
    fn global_and_repo_context_render_with_their_container_paths() {
        let collected = CollectedOverlays {
            context_overlays: vec![
                ContextOverlaySpec {
                    scope: ContextScope::Global,
                    permission: OverlayPermission::ReadWrite,
                },
                ContextOverlaySpec {
                    scope: ContextScope::Repo,
                    permission: OverlayPermission::ReadOnly,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            inventory(&collected, &nothing_set).context,
            "- `context(global)` → `/awman/context/global` (read-write)\n\
             - `context(repo)` → `/awman/context/repo` (read-only)"
        );
    }

    #[test]
    fn workflow_context_is_left_to_the_always_present_section() {
        let collected = CollectedOverlays {
            context_overlays: vec![ContextOverlaySpec {
                scope: ContextScope::Workflow,
                permission: OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        };
        assert_eq!(
            inventory(&collected, &nothing_set).context,
            "(none)",
            "context(workflow) is the durable task workspace, already reported \
             under the template's always-present mounts"
        );
    }
}
