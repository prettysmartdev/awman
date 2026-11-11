//! Host-side orchestration for pulling managed skill libraries.

use std::path::{Component, Path, PathBuf};

use crate::command::error::CommandError;
use crate::data::fs::skill_library::{
    parse_github_slug, read_library_meta, write_library_meta, GithubSlug, SkillLibraryMeta,
    LIBRARY_META_FILENAME,
};
use crate::data::fs::SkillDirs;
use crate::data::session::AgentName;
use crate::engine::git::{GitEngine, SparseCloneOutcome, SPARSE_FALLBACK_WARNING};

/// Names that select the skill library bundled in the awman repository.
pub const BUILTIN_LIBRARY_ALIASES: &[&str] = &["builtin", "awman"];
pub const BUILTIN_LIBRARY_OWNER: &str = "prettysmartdev";
pub const BUILTIN_LIBRARY_REPO: &str = "awman";
pub const BUILTIN_LIBRARY_SUBDIR: &str = "skills";

/// The two forms accepted by `new skill --pull`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullTarget {
    /// A complete GitHub owner/repository slug, for a new pull or explicit refresh.
    Slug(GithubSlug),
    /// The short name of an already-pulled library.
    ExistingByName(String),
    /// `builtin` / `awman`: the prettysmartdev/awman `skills/` tree, cloned sparse and shallow.
    Builtin,
}

/// Resolve a `--pull` argument without looking at the filesystem.
pub fn resolve_pull_target(input: &str) -> Result<PullTarget, String> {
    if input.contains('/') || input.contains("github.com") {
        return parse_github_slug(input).map(PullTarget::Slug);
    }

    let name = input.trim();
    if BUILTIN_LIBRARY_ALIASES.contains(&name) {
        return Ok(PullTarget::Builtin);
    }
    AgentName::new(name).map_err(|error| error.to_string())?;
    Ok(PullTarget::ExistingByName(name.to_string()))
}

/// A successfully cloned or refreshed library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    pub slug: String,
    pub dir: PathBuf,
    pub subdir: String,
    pub skills_found: Vec<String>,
    pub was_update: bool,
    /// The library is a shallow, sparse clone.
    pub sparse: bool,
    /// Set when git was too old for sparse checkout and a plain shallow clone was made.
    pub sparse_fallback_warning: Option<String>,
}

/// Clone or refresh one managed skill library.
pub fn pull_library(
    git_engine: &GitEngine,
    skill_dirs: &SkillDirs,
    target: PullTarget,
    subdir_override: Option<&str>,
) -> Result<PullOutcome, CommandError> {
    if let Some(subdir) = subdir_override {
        validate_subdir(subdir)?;
    }
    let builtin = target == PullTarget::Builtin;
    if builtin && subdir_override.is_some() {
        return Err(CommandError::Other(
            "--subdir cannot be combined with the builtin skill library".to_string(),
        ));
    }

    let (owner, repo, slug, clone_url) = match target {
        PullTarget::Builtin => {
            let clone_url =
                format!("https://github.com/{BUILTIN_LIBRARY_OWNER}/{BUILTIN_LIBRARY_REPO}.git");
            (
                BUILTIN_LIBRARY_OWNER.to_string(),
                BUILTIN_LIBRARY_REPO.to_string(),
                BUILTIN_LIBRARY_REPO.to_string(),
                clone_url,
            )
        }
        PullTarget::Slug(slug) => {
            let clone_url = format!("https://github.com/{}/{}.git", slug.owner, slug.repo);
            (slug.owner, slug.repo.clone(), slug.repo, clone_url)
        }
        PullTarget::ExistingByName(name) => {
            AgentName::new(&name).map_err(|error| CommandError::Other(error.to_string()))?;
            let dir = skill_dirs.library_dir(&name);
            ensure_not_symlinked(&dir, &name)?;
            if !dir.exists() || !dir.join(LIBRARY_META_FILENAME).is_file() {
                return Err(CommandError::Other(format!(
                    "library '{name}' has not been pulled yet; use the full GitHub slug, e.g. --pull github.com/<owner>/{name}"
                )));
            }
            let meta = read_library_meta(&dir)?;
            (meta.owner, meta.repo, name, meta.source)
        }
    };

    let dir = skill_dirs.library_dir(&slug);
    // A symlink anywhere in the managed surface would let the refresh's hard
    // reset escape `.library/` and destroy content awman does not own, so it is
    // rejected before `dir.exists()` is ever consulted (that predicate follows
    // symlinks).
    ensure_not_symlinked(&dir, &slug)?;
    let mut sparse_fallback_warning = None;
    let (meta, was_update) = if !dir.exists() {
        let subdir = if builtin {
            BUILTIN_LIBRARY_SUBDIR
        } else {
            subdir_override.unwrap_or("skills")
        };
        if builtin {
            if git_engine.clone_repo_sparse(&clone_url, &dir, subdir)?
                == SparseCloneOutcome::FellBackToShallow
            {
                sparse_fallback_warning = Some(SPARSE_FALLBACK_WARNING.to_string());
            }
        } else {
            git_engine.clone_repo(&clone_url, None, &dir)?;
        }
        let meta = SkillLibraryMeta {
            source: clone_url,
            owner,
            repo,
            subdir: subdir.to_string(),
            sparse: builtin,
        };
        write_library_meta(&dir, &meta)?;
        (meta, false)
    } else {
        if !dir.join(LIBRARY_META_FILENAME).is_file() || !dir.join(".git").is_dir() {
            return Err(CommandError::Other(format!(
                "{} exists but is not an awman-managed skill library; remove it manually before pulling '{}' here",
                dir.display(),
                slug
            )));
        }

        let mut meta = read_library_meta(&dir)?;
        if meta.owner != owner || meta.repo != repo {
            return Err(CommandError::Other(format!(
                "library '{slug}' was already pulled from {}/{}; refusing to overwrite with {owner}/{repo}. Remove {} first if you want to replace it.",
                meta.owner,
                meta.repo,
                dir.display()
            )));
        }
        validate_subdir(&meta.subdir)?;
        // `.awman.json` is the recorded source of truth, but `pull_latest`
        // fetches whatever `origin` currently points at. Refuse to refresh a
        // clone whose remote has drifted away from the recorded source rather
        // than hard-resetting it to content from an unrecorded upstream.
        let origin = git_engine.remote_url(&dir, "origin")?;
        if normalize_remote_url(&origin) != normalize_remote_url(&meta.source) {
            return Err(CommandError::Other(format!(
                "library '{slug}' has a git origin ({origin}) that does not match its recorded source ({}); refusing to refresh. Remove {} first if you want to re-pull it.",
                meta.source,
                dir.display()
            )));
        }
        if meta.sparse {
            git_engine.pull_latest_shallow(&dir)?;
        } else {
            git_engine.pull_latest(&dir)?;
        }
        if let Some(subdir) = subdir_override {
            meta.subdir = subdir.to_string();
        }
        // A hard reset may have restored an upstream-tracked metadata file.
        // Reassert awman's retained metadata after every managed refresh.
        write_library_meta(&dir, &meta)?;
        (meta, true)
    };

    validate_subdir(&meta.subdir)?;
    let skills_dir = dir.join(&meta.subdir);
    if !skills_dir.is_dir() {
        return Err(CommandError::Other(format!(
            "subdirectory '{}' not found in {slug}; pass --subdir to point at the folder containing SKILL.md directories",
            meta.subdir
        )));
    }

    let skills_found = discover_skills(&skills_dir)?;
    if skills_found.is_empty() {
        tracing::debug!(
            "skill library '{}' has no immediate SKILL.md children under {}",
            slug,
            skills_dir.display()
        );
    }
    Ok(PullOutcome {
        slug,
        dir,
        subdir: meta.subdir,
        skills_found,
        was_update,
        sparse: meta.sparse,
        sparse_fallback_warning,
    })
}

/// Refresh every valid, persisted library in deterministic slug order.
pub fn pull_all_libraries(
    git_engine: &GitEngine,
    skill_dirs: &SkillDirs,
) -> Vec<Result<PullOutcome, CommandError>> {
    skill_dirs
        .list_libraries()
        .into_iter()
        .map(|slug| {
            pull_library(
                git_engine,
                skill_dirs,
                PullTarget::ExistingByName(slug),
                None,
            )
        })
        .collect()
}

/// Reject a library path that is (or whose managed marker files are) a
/// symlink.
///
/// `Path::exists`/`is_dir`/`is_file` all traverse symlinks, so without this
/// guard a symlink planted at `.library/<slug>` — or at its `.git` /
/// `.awman.json` — would make `pull_library` treat an arbitrary external
/// checkout as managed content and hard-reset it. awman never deletes or
/// overwrites anything outside `.library/<slug>`.
fn ensure_not_symlinked(dir: &Path, slug: &str) -> Result<(), CommandError> {
    for candidate in [
        dir.to_path_buf(),
        dir.join(".git"),
        dir.join(LIBRARY_META_FILENAME),
    ] {
        if crate::data::fs::path_guard::ensure_not_symlinked(&candidate).is_err() {
            return Err(CommandError::Other(format!(
                "{} is a symlink, not an awman-managed skill library; remove it manually before pulling '{slug}' here",
                candidate.display()
            )));
        }
    }
    Ok(())
}

/// Normalize a git remote URL for comparison: trailing whitespace, a trailing
/// `/`, and a trailing `.git` are all insignificant.
fn normalize_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    trimmed.strip_suffix(".git").unwrap_or(trimmed).to_string()
}

fn validate_subdir(subdir: &str) -> Result<(), CommandError> {
    if subdir.is_empty() {
        return Err(CommandError::Other(
            "skill library subdirectory must be a non-empty relative path".to_string(),
        ));
    }
    let path = Path::new(subdir);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::CurDir
                    | Component::ParentDir
            )
        })
    {
        return Err(CommandError::Other(format!(
            "skill library subdirectory '{subdir}' must be a relative path inside the clone"
        )));
    }
    Ok(())
}

fn discover_skills(skills_dir: &Path) -> Result<Vec<String>, CommandError> {
    let entries = std::fs::read_dir(skills_dir)
        .map_err(|error| crate::data::error::DataError::io(skills_dir, error))?;
    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| crate::data::error::DataError::io(skills_dir, error))?;
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            skills.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    skills.sort();
    Ok(skills)
}
