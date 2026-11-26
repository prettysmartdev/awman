//! Preflight (design 3.2, 7.5, 7.9, 7.11). Two parts, both stop at the
//! first failure with one primary error and one recovery:
//!
//! * `admit` — only for a new attempt: prepared source, version, notes,
//!   free version, main ancestry.
//! * `check` — the prerequisites of the phases that are still unfinished,
//!   so a publication or verification retry never needs a native host,
//!   fixtures or a toolchain that completed work already used.

use super::error::{stop, Step, Stop};
use super::forge::RepoInfo;
use super::plan::Plan;
use super::world::{Head, World};
use std::collections::BTreeSet;
use std::path::Path;

pub const NOTES_PLACEHOLDER: &str = "_Write release notes here._";

/// `version = "X"` of `[workspace.package]` (or the first `version`).
pub fn cargo_version(toml: &str) -> Option<String> {
    let mut in_ws = false;
    let mut first = None;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_ws = t == "[workspace.package]";
            continue;
        }
        if let Some(rest) = t.strip_prefix("version") {
            let v = rest
                .trim_start()
                .strip_prefix('=')
                .map(|v| v.trim().trim_matches('"').to_string());
            if in_ws {
                return v;
            }
            if first.is_none() {
                first = v;
            }
        }
    }
    first
}

pub fn version_at(w: &World, commit: &str) -> Step<String> {
    let toml = w
        .source
        .file_at(commit, "Cargo.toml")?
        .ok_or_else(|| Stop::new("version-mismatch", "Cargo.toml is missing at HEAD"))?;
    cargo_version(&String::from_utf8_lossy(&toml))
        .map(|v| format!("v{v}"))
        .ok_or_else(|| Stop::new("version-mismatch", "Cargo.toml has no version"))
}

/// The side-effect-free version probe of each tool. OpenSSH has no
/// `--version` (it exits 255); `ssh -V` is its only version flag.
pub fn probe_args(tool: &str) -> &'static [&'static str] {
    match tool {
        "ssh" => &["-V"],
        _ => &["--version"],
    }
}

/// Tools the production controller itself invokes for the unfinished phases.
pub fn tools(w: &World, need_ssh: bool) -> Step {
    if !w.production {
        return Ok(());
    }
    let mut needed = vec!["git", "gh", "curl"];
    if need_ssh {
        needed.push("ssh");
    }
    for tool in needed {
        let ok = std::process::Command::new(tool)
            .args(probe_args(tool))
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            stop!("tool-missing", "`{tool}` is not installed or not on PATH");
        }
    }
    Ok(())
}

/// Admission of a new attempt. Returns HEAD (the source commit R) and the
/// fetched default-branch tip M.
/// `tag` is the name this attempt will create: the version, or a
/// disposable rehearsal's `rehearsal-*` name.
pub fn admit(
    w: &World,
    repo: &RepoInfo,
    version: &str,
    tag: &str,
    rehearsal: bool,
) -> Step<(Head, String)> {
    let head = w.source.head()?;
    let release_branch = format!("release/{version}");
    if !rehearsal && head.branch != repo.default_branch && head.branch != release_branch {
        return Err(Stop::new(
            "wrong-branch",
            format!(
                "HEAD is on `{}`; a release starts on {} or {release_branch}",
                head.branch, repo.default_branch
            ),
        ));
    }
    if !head.clean {
        stop!(
            "dirty-checkout",
            "the checkout has uncommitted changes to tracked files"
        );
    }
    let semver = version.trim_start_matches('v');
    let toml = w
        .source
        .file_at(&head.commit, "Cargo.toml")?
        .unwrap_or_default();
    if cargo_version(&String::from_utf8_lossy(&toml)).as_deref() != Some(semver) {
        stop!(
            "version-mismatch",
            "Cargo.toml at {} is not version {semver}",
            &head.commit[..12]
        );
    }
    let lock = w
        .source
        .file_at(&head.commit, "Cargo.lock")?
        .unwrap_or_default();
    if !String::from_utf8_lossy(&lock).contains(&format!("version = \"{semver}\"")) {
        stop!(
            "version-mismatch",
            "Cargo.lock at {} does not record version {semver}",
            &head.commit[..12]
        );
    }
    if !rehearsal {
        let notes_path = format!("docs/releases/{version}.md");
        match w.source.file_at(&head.commit, &notes_path)? {
            Some(n)
                if !n.is_empty() && !String::from_utf8_lossy(&n).contains(NOTES_PLACEHOLDER) => {}
            Some(_) => stop!(
                "notes-missing",
                "{notes_path} still contains the placeholder"
            ),
            None => stop!(
                "notes-missing",
                "{notes_path} is missing at {}",
                &head.commit[..12]
            ),
        }
        if let Some(existing) = w.forge.tag(tag)? {
            stop!(
                "version-taken",
                "tag {tag} already exists at {}",
                &existing.commit[..12.min(existing.commit.len())]
            );
        }
        // A new attempt never adopts an existing draft: only a recorded
        // attempt (journal or claim) owns one.
        for r in w.forge.releases()? {
            if r.tag == tag {
                return Err(Stop::new(
                    "version-taken",
                    format!("a {} release for {tag} already exists (id {})", if r.draft { "draft" } else { "published" }, r.id),
                )
                .with_remedy(if r.draft {
                    format!("check draft {}; delete it if it is a leftover, or resume from the clone that owns it", r.id)
                } else {
                    "choose a new version; a published release is never reused".to_string()
                }));
            }
        }
    }
    let main = w.source.fetch(&repo.default_branch)?;
    if !w.source.is_ancestor(&main, &head.commit)? {
        stop!(
            "base-not-contained",
            "origin/{} ({}) is not an ancestor of HEAD {}",
            repo.default_branch,
            &main[..12],
            &head.commit[..12]
        );
    }
    Ok((head, main))
}

pub struct Remaining<'a> {
    pub phases: BTreeSet<&'static str>,
    pub natives: Vec<&'a str>,
}

/// Prerequisites of the unfinished phases, in a fixed order.
#[allow(clippy::too_many_arguments)]
pub fn check(
    w: &World,
    repo: &RepoInfo,
    plan: &Plan,
    source_dir: &Path,
    version: &str,
    rehearsal: bool,
    remaining: &Remaining,
    linux_host: Option<&str>,
    fixture_cache: &Path,
) -> Step {
    let any = |p: &[&str]| p.iter().any(|x| remaining.phases.contains(x));
    let ssh = remaining
        .natives
        .iter()
        .any(|id| plan.native(id).is_some_and(|n| n.host == "ssh"));
    tools(w, ssh)?;
    if any(&["dispatch", "collect"]) {
        for wf in [plan.ci.workflow.as_str(), plan.release_workflow.as_str()] {
            if !w.forge.workflow_active(wf)? {
                stop!(
                    "workflow-missing",
                    "{wf} is not active on {}",
                    repo.default_branch
                );
            }
        }
        super::workflows::closure(&source_dir.join(".github/workflows"))?;
    }
    if !rehearsal && any(&["land", "tag", "publish"]) {
        let rules = w.forge.blocking_rules(&repo.default_branch, version)?;
        if let Some(rule) = rules.first() {
            stop!(
                "publication-policy-denied",
                "{rule} would stop this login from landing or tagging"
            );
        }
    }
    for id in &remaining.natives {
        let Some(n) = plan.native(id) else { continue };
        let host = w.hosts.host(n, linux_host)?;
        host.probe(n)?;
        if n.host == "local" {
            super::native::fetch_downloads(w.forge.as_ref(), fixture_cache, n)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_version_wins_over_dependency_versions() {
        let toml = "[package]\nversion = \"9.9.9\"\n[workspace.package]\nversion = \"0.15.0\"\n";
        assert_eq!(cargo_version(toml).as_deref(), Some("0.15.0"));
        assert_eq!(
            cargo_version("[package]\nversion = \"1.0.0\"\n").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(cargo_version("[package]\nname = \"x\"\n"), None);
    }

    #[test]
    fn every_probed_tool_uses_a_version_flag_it_accepts() {
        // `ssh --version` exits 255 on OpenSSH; the probe must use `-V`.
        assert_eq!(probe_args("ssh"), ["-V"]);
        for tool in ["git", "gh", "curl"] {
            assert_eq!(probe_args(tool), ["--version"], "{tool}");
        }
    }
}
