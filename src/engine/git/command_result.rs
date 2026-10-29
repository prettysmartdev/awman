//! Read-only checkout facts for a command result. Never refresh or stage the index.
use std::collections::HashMap;
use std::path::Path;

use super::{host_command, GitEngine};
use crate::data::fs::api_db::{ChangedFile, ChangedFileStatus, GitResult, CHANGED_FILES_CAP};
use crate::engine::error::EngineError;

fn read(root: &Path, args: &[&str]) -> Result<Vec<u8>, EngineError> {
    let output = host_command::command(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .map_err(|error| EngineError::Git(error.to_string()))?;
    if !output.status.success() {
        return Err(EngineError::Git(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    Ok(output.stdout)
}

fn text(root: &Path, args: &[&str]) -> Option<String> {
    let bytes = read(root, args).ok()?;
    let value = String::from_utf8_lossy(&bytes).trim().to_string();
    (!value.is_empty()).then_some(value)
}

impl GitEngine {
    /// HEAD before dispatch. None also covers a non-git session or an unborn HEAD.
    pub fn command_base_sha(&self, root: &Path) -> Option<String> {
        text(root, &["rev-parse", "--verify", "HEAD"])
    }

    /// Compare the initial commit with the final working tree, including dirty
    /// and untracked files. NUL-delimited output preserves tabs, newlines and
    /// rename paths without interpreting git's quoted presentation format.
    pub fn command_result(
        &self,
        root: &Path,
        base: Option<&str>,
    ) -> Result<Option<GitResult>, EngineError> {
        if text(root, &["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
            return Ok(None);
        }
        let mut result = GitResult {
            branch: text(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]),
            base_sha: base.map(str::to_owned),
            head_sha: self.command_base_sha(root),
            ..GitResult::default()
        };
        let untracked = read(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        // --no-optional-locks/GIT_OPTIONAL_LOCKS=0 prevents status's optional
        // index refresh. Status also handles unborn branches and staged edits.
        result.dirty = !read(root, &["status", "--porcelain", "-z"])?.is_empty();
        // An unborn branch still has staged/tracked changes. Compute the
        // empty tree id without modifying the index or object database.
        let empty_tree = if base.is_none() {
            text(root, &["hash-object", "-t", "tree", "--stdin"])
        } else {
            None
        };
        if let Some(base) = base.or(empty_tree.as_deref()) {
            let stats = read(
                root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--numstat",
                    "-z",
                    "-M",
                    base,
                    "--",
                ],
            );
            let names = read(
                root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--name-status",
                    "-z",
                    "-M",
                    base,
                    "--",
                ],
            );
            let (stats, names) = match (stats, names) {
                (Ok(stats), Ok(names)) => (stats, names),
                (Err(error), _) | (_, Err(error)) => {
                    result.changed_files_error = Some(error.to_string());
                    return Ok(Some(result));
                }
            };
            let mut kinds = HashMap::new();
            let mut tokens = names.split(|b| *b == 0);
            while let Some(kind) = tokens.next().filter(|s| !s.is_empty()) {
                let Some(first) = tokens.next() else { break };
                let (status, old_path, path) = match kind[0] {
                    b'R' | b'C' => {
                        let Some(path) = tokens.next() else { break };
                        (
                            if kind[0] == b'R' {
                                ChangedFileStatus::Renamed
                            } else {
                                ChangedFileStatus::Copied
                            },
                            Some(String::from_utf8_lossy(first).into_owned()),
                            path,
                        )
                    }
                    b'A' => (ChangedFileStatus::Added, None, first),
                    b'D' => (ChangedFileStatus::Deleted, None, first),
                    b'T' => (ChangedFileStatus::TypeChanged, None, first),
                    b'U' => (ChangedFileStatus::Unmerged, None, first),
                    _ => (ChangedFileStatus::Modified, None, first),
                };
                kinds.insert(path.to_vec(), (status, old_path));
            }
            let mut tokens = stats.split(|b| *b == 0);
            while let Some(record) = tokens.next().filter(|s| !s.is_empty()) {
                let mut fields = record.splitn(3, |b| *b == b'\t');
                let added = fields
                    .next()
                    .and_then(|n| std::str::from_utf8(n).ok()?.parse().ok());
                let deleted = fields
                    .next()
                    .and_then(|n| std::str::from_utf8(n).ok()?.parse().ok());
                let Some(mut path) = fields.next() else {
                    continue;
                };
                if path.is_empty() {
                    // Rename numstat: counts<TAB><NUL>old<NUL>new<NUL>.
                    let _old = tokens.next();
                    let Some(new) = tokens.next() else { break };
                    path = new;
                }
                let (status, old_path) = kinds
                    .remove(path)
                    .unwrap_or((ChangedFileStatus::Modified, None));
                result.changed_files.push(ChangedFile {
                    path: String::from_utf8_lossy(path).into_owned(),
                    status,
                    old_path,
                    added,
                    deleted,
                });
                // One entry past the cap, so a list of exactly CAP entries is
                // not mistaken for a truncated one.
                if result.changed_files.len() > CHANGED_FILES_CAP {
                    break;
                }
            }
        }
        for path in untracked.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            if result.changed_files.len() > CHANGED_FILES_CAP {
                break;
            }
            result.changed_files.push(ChangedFile {
                path: String::from_utf8_lossy(path).into_owned(),
                status: ChangedFileStatus::Added,
                old_path: None,
                added: None,
                deleted: None,
            });
        }
        result.changed_files_truncated = result.changed_files.len() > CHANGED_FILES_CAP;
        result.changed_files.truncate(CHANGED_FILES_CAP);
        Ok(Some(result))
    }
}
