//! Metadata and GitHub-slug parsing for pulled skill libraries.
//!
//! A "skill library" is a git clone of a published skills repository (e.g.
//! `github.com/obra/superpowers`) living under
//! `SkillDirs::library_dir(slug)`. This module owns the pure data shape of
//! that clone's persisted metadata file and the parsing of the GitHub
//! owner/repo slugs used to pull one. It has no git/engine/command
//! dependencies, matching the Layer 0 boundary of `src/data/fs/`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::config_file::write_config_file_atomically;
use crate::data::session::AgentName;

/// Filename of the persisted metadata file inside a pulled library directory.
pub const LIBRARY_META_FILENAME: &str = ".awman.json";

/// Persisted metadata describing where a pulled skill library came from and
/// which subdirectory inside it holds skills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillLibraryMeta {
    pub source: String,
    pub owner: String,
    pub repo: String,
    #[serde(default = "default_subdir")]
    pub subdir: String,
    /// The clone is shallow and sparse (only `subdir` is checked out), so
    /// refreshes must stay shallow too. Legacy files deserialize as `false`.
    #[serde(default)]
    pub sparse: bool,
}

fn default_subdir() -> String {
    "skills".to_string()
}

/// Read a library's persisted metadata from `<library_dir>/.awman.json`.
///
/// A missing file surfaces as `DataError::Io`, not a default value — callers
/// that want "not pulled yet" semantics must check for that themselves.
pub fn read_library_meta(library_dir: &Path) -> Result<SkillLibraryMeta, DataError> {
    let path = library_dir.join(LIBRARY_META_FILENAME);
    let content = std::fs::read_to_string(&path).map_err(|e| DataError::io(&path, e))?;
    serde_json::from_str(&content).map_err(|e| DataError::config_parse(&path, e))
}

/// Write a library's metadata to `<library_dir>/.awman.json`, creating the
/// library directory if it does not already exist.
pub fn write_library_meta(library_dir: &Path, meta: &SkillLibraryMeta) -> Result<(), DataError> {
    std::fs::create_dir_all(library_dir).map_err(|e| DataError::io(library_dir, e))?;
    let path = library_dir.join(LIBRARY_META_FILENAME);
    let content =
        serde_json::to_string_pretty(meta).map_err(|e| DataError::ConfigSerialize { source: e })?;
    // Atomic: overlay resolution reads this file and fails on a partial one.
    write_config_file_atomically(&path, &content)
}

/// A parsed `<owner>/<repo>` GitHub slug, sanitized for use as a directory
/// name and inside `skill()` overlay expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubSlug {
    pub owner: String,
    pub repo: String,
}

/// Parse a GitHub repo reference into an owner/repo slug.
///
/// Accepts, in order of precedence:
/// - `https://github.com/<owner>/<repo>` or `http://github.com/<owner>/<repo>`
///   (optionally with a trailing `.git` or `/`)
/// - `github.com/<owner>/<repo>`
/// - `<owner>/<repo>` (bare short form)
///
/// A bare single-segment input (no `/`, e.g. `superpowers`) is rejected here —
/// it is a re-pull-by-name request, resolved elsewhere against already-pulled
/// libraries, not an owner/repo pair.
pub fn parse_github_slug(input: &str) -> Result<GithubSlug, String> {
    let trimmed = input.trim();
    let without_scheme = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("github.com/"))
        .unwrap_or(trimmed);
    let without_trailing_slash = without_scheme.strip_suffix('/').unwrap_or(without_scheme);

    let segments: Vec<&str> = without_trailing_slash.split('/').collect();
    if segments.len() != 2 {
        return Err(format!(
            "expected a GitHub owner/repo slug (e.g. \"github.com/<owner>/<repo>\" or \"<owner>/<repo>\"), got '{input}'"
        ));
    }

    let owner = segments[0];
    let repo_raw = segments[1];
    if owner.is_empty() || repo_raw.is_empty() {
        return Err(format!(
            "owner and repo must not be empty in GitHub slug '{input}'"
        ));
    }

    let repo_no_git = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);
    let repo = repo_no_git.to_lowercase();

    AgentName::new(&repo).map_err(|e| match e {
        DataError::InvalidAgentName { reason, .. } => {
            format!("invalid repo name '{repo_raw}' in GitHub slug '{input}': {reason}")
        }
        other => other.to_string(),
    })?;

    Ok(GithubSlug {
        owner: owner.to_string(),
        repo,
    })
}
