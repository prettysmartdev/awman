//! A squad directory task's task-owned image context (WI 0105) — Layer 0.
//!
//! Every [`MountScope::Directory`](crate::data::fs::task_store::MountScope)
//! task builds its images from its guarded task folder,
//! `<squad root>/tasks/<name>/workspace/`: `Dockerfile.dev` and
//! `.awman/Dockerfile.<agent>` live there, the folder is the build context,
//! and it is the folder mounted at `/awman/context/workflow`. A custom
//! non-Git task keeps its chosen directory only as `/workspace`; awman never
//! builds from it, writes into it, or looks above it.
//!
//! Every read and create below the context goes through [`RepoTree`], which
//! walks one component at a time without following links, so a symlinked
//! `workspace`, `.awman` or recipe — or one swapped in between check and use
//! — is refused rather than followed.
//!
//! The legacy migration is plan-then-apply: [`TaskImageContext::plan_legacy_migration`]
//! only reads, and [`TaskImageContext::apply_legacy_migration`] only creates
//! files that are missing, so a refused plan changes nothing and an
//! interrupted apply is safely retried.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::path_guard::{ensure_not_symlinked, validate_under_root};
use crate::data::fs::repo_tree::{CreateOutcome, RepoEntry, RepoRelPath, RepoTree, RepoTreeError};
use crate::data::image_inputs::{resolve_copy_sources, CopySource, UnresolvedReason};
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;

use super::SquadPaths;

/// The migration marker's file name, a sibling of the task's `config.json`.
pub const TASK_IMAGE_STATE_FILE: &str = "image-context.json";

/// Most build-input files one legacy migration copies.
pub const MAX_LEGACY_INPUT_FILES: usize = 2_000;

/// Most build-input bytes one legacy migration copies.
pub const MAX_LEGACY_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// The project recipe's path inside an image context.
pub const PROJECT_RECIPE: &str = "Dockerfile.dev";

/// One task's validated image context.
#[derive(Debug, Clone)]
pub struct TaskImageContext {
    task: String,
    /// `<squad root>/tasks/<name>`.
    task_root: PathBuf,
    /// `<squad root>/tasks/<name>/workspace` — the image root.
    root: PathBuf,
}

impl SquadPaths {
    /// The task-owned image context for `name`, created if missing.
    ///
    /// `name` must be one plain path component that stays under the tasks
    /// root. `tasks/<name>`, its `workspace` and `workspace/.awman` must not
    /// be symbolic links, and the canonical context must still be
    /// `<tasks>/<name>/workspace` once it exists, so a swapped-in link cannot
    /// redirect the image root.
    pub fn task_image_context(&self, name: &str) -> Result<TaskImageContext, DataError> {
        let mut components = Path::new(name).components();
        let single = matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(part)), None) if part == std::ffi::OsStr::new(name)
        );
        if !single {
            return Err(DataError::InvalidPath {
                path: self.tasks_dir().join(name),
                reason: "a task name must be a single plain path component".into(),
            });
        }
        let task_root = self.tasks_dir().join(name);
        validate_under_root(
            &self.tasks_dir(),
            &task_root,
            "task directory must reside under the squad tasks root",
        )?;
        let root = crate::data::checkout_paths::squad_workspace_path(&task_root);
        ensure_not_symlinked(&task_root)?;
        ensure_not_symlinked(&root)?;
        ensure_not_symlinked(&root.join(".awman"))?;
        std::fs::create_dir_all(&root).map_err(|e| DataError::io(&root, e))?;
        let context = TaskImageContext {
            task: name.to_string(),
            task_root,
            root,
        };
        context.tree()?;
        Ok(context)
    }
}

/// The migration marker: the custom directory's legacy recipes were checked
/// once and either migrated or found absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskImageState {
    pub legacy_checked: bool,
    pub from: PathBuf,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// What a legacy file is to the image it builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyKind {
    Recipe,
    BuildInput,
    DockerIgnore,
    /// A directory a recipe copies (created even when empty, so `COPY dir`
    /// keeps working).
    Directory,
}

/// One file to copy from the custom directory to the same relative path in
/// the task context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyCopy {
    /// `/`-separated, relative to both roots.
    pub rel: String,
    pub kind: LegacyKind,
    /// SHA-256 of the bytes the plan approved (`None` for a directory). Apply
    /// copies only those bytes and accepts an existing destination only when
    /// it holds them.
    pub digest: Option<String>,
}

/// Why a legacy recipe cannot be moved without changing its image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyRefusal {
    /// `COPY .` — the image depends on the whole custom directory.
    WholeContext,
    /// A wildcard, variable or unparsable source.
    Unresolvable,
    /// A source naming a path above the custom directory.
    EscapesDirectory,
    /// A recipe, an input or a path to one is a link or not a regular file.
    Symlink,
    /// More than [`MAX_LEGACY_INPUT_FILES`] files or [`MAX_LEGACY_INPUT_BYTES`].
    TooLarge,
    /// The task context already holds different bytes at an input's path.
    DestinationDiffers,
}

impl LegacyRefusal {
    fn describe(self) -> &'static str {
        match self {
            Self::WholeContext => "it copies the whole directory",
            Self::Unresolvable => "the source uses a wildcard or variable awman cannot resolve",
            Self::EscapesDirectory => "the source is outside that directory",
            Self::Symlink => "the path is a symbolic link or not a regular file",
            Self::TooLarge => "the inputs exceed the migration limit (2000 files / 64 MiB)",
            Self::DestinationDiffers => "the task context already has a different file there",
        }
    }
}

/// The result of planning a legacy migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyRecipePlan {
    /// The custom directory holds none of the needed recipes.
    Nothing,
    /// Copy `files` (create-if-missing). `kept` names legacy recipes that a
    /// task-owned file already supersedes.
    Copy {
        files: Vec<LegacyCopy>,
        kept: Vec<String>,
    },
    /// Nothing may change: `recipe` needs `input`, which cannot be moved.
    Refuse {
        recipe: String,
        input: String,
        why: LegacyRefusal,
    },
}

impl LegacyRecipePlan {
    /// The actionable message for a refused plan; `None` otherwise.
    pub fn refusal_message(
        &self,
        task: &str,
        legacy_root: &Path,
        context: &Path,
    ) -> Option<String> {
        let Self::Refuse { recipe, input, why } = self else {
            return None;
        };
        let what = if recipe == input {
            format!("{recipe} in {}", legacy_root.display())
        } else {
            format!(
                "{recipe} in {} copies \"{input}\" from its build context",
                legacy_root.display()
            )
        };
        Some(format!(
            "Task \"{task}\": {what} ({}). awman now builds this task's images from {} and \
             cannot move it safely. Copy the recipe and the files it needs into {} (or remove \
             the COPY), then trigger the task again. Nothing was changed.",
            why.describe(),
            context.display(),
            context.display(),
        ))
    }
}

fn tree_error(error: RepoTreeError) -> DataError {
    DataError::Other(error.to_string())
}

fn rel(path: &str) -> Result<RepoRelPath, DataError> {
    RepoRelPath::new(Path::new(path)).ok_or_else(|| DataError::InvalidPath {
        path: path.into(),
        reason: "not a plain relative path".into(),
    })
}

fn agent_recipe(agent: &str) -> Result<String, DataError> {
    crate::data::session::AgentName::new(agent)?;
    Ok(format!(".awman/Dockerfile.{agent}"))
}

impl TaskImageContext {
    /// The image root: Dockerfile location and build context.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    /// The recipe paths below the root.
    pub fn dockerfiles(&self) -> RepoDockerfilePaths {
        RepoDockerfilePaths::new(&self.root)
    }

    /// `<squad root>/tasks/<name>/image-context.json`.
    pub fn state_file(&self) -> PathBuf {
        self.task_root.join(TASK_IMAGE_STATE_FILE)
    }

    /// Open the context for a link-refusing walk, re-checking that the
    /// canonical root is still the task's own folder.
    fn tree(&self) -> Result<RepoTree, DataError> {
        ensure_not_symlinked(&self.task_root)?;
        ensure_not_symlinked(&self.root)?;
        let tree = RepoTree::open(&self.root).map_err(tree_error)?;
        let parent = self
            .task_root
            .parent()
            .and_then(|tasks| std::fs::canonicalize(tasks).ok());
        let expected = parent.map(|tasks| {
            crate::data::checkout_paths::squad_workspace_path(&tasks.join(&self.task))
        });
        if expected.as_deref() != Some(tree.root()) {
            return Err(DataError::InvalidPath {
                path: self.root.clone(),
                reason: format!(
                    "the task image context resolves to {}, not the task's own folder",
                    tree.root().display()
                ),
            });
        }
        Ok(tree)
    }

    /// Create `Dockerfile.dev` from `template` unless a regular file is
    /// already there. An edited file is kept byte-for-byte; a link (live or
    /// dangling) or anything else at the path is refused.
    pub fn ensure_project_recipe(&self, template: &str) -> Result<CreateOutcome, DataError> {
        self.tree()?
            .create_new(&rel(PROJECT_RECIPE)?, template.as_bytes())
            .map_err(tree_error)
    }

    /// Refuse the run when `Dockerfile.dev` or a needed agent recipe is not
    /// a regular file reachable without following a link. Missing recipes
    /// pass: scaffolding or the build reports them.
    pub fn verify_recipes(&self, agents: &[String]) -> Result<(), DataError> {
        let tree = self.tree()?;
        let mut paths = vec![PROJECT_RECIPE.to_string()];
        for agent in agents {
            paths.push(agent_recipe(agent)?);
        }
        for path in paths {
            match tree.read(&rel(&path)?) {
                Ok(RepoEntry::File(_) | RepoEntry::Missing) => {}
                Ok(RepoEntry::Other(kind)) => {
                    return Err(DataError::InvalidPath {
                        path: self.root.join(&path),
                        reason: format!("is a {kind}; image recipes must be regular files"),
                    })
                }
                Err(error) => return Err(tree_error(error)),
            }
        }
        Ok(())
    }

    /// The migration marker, when one was written.
    pub fn load_state(&self) -> Result<Option<TaskImageState>, DataError> {
        let path = self.state_file();
        ensure_not_symlinked(&path)?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| DataError::Other(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DataError::io(&path, e)),
        }
    }

    /// Write the migration marker atomically. Written last, after every copy.
    pub fn record_state(&self, from: &Path) -> Result<(), DataError> {
        let path = self.state_file();
        ensure_not_symlinked(&path)?;
        let state = TaskImageState {
            legacy_checked: true,
            from: from.to_path_buf(),
            at: chrono::Utc::now(),
        };
        let bytes = serde_json::to_vec_pretty(&state)
            .map_err(|e| DataError::Other(format!("{}: {e}", path.display())))?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.task_root)
            .map_err(|e| DataError::io(&self.task_root, e))?;
        std::io::Write::write_all(&mut temp, &bytes).map_err(|e| DataError::io(&path, e))?;
        temp.persist(&path)
            .map_err(|e| DataError::io(&path, e.error))?;
        Ok(())
    }

    /// Plan moving the needed recipes (and the local inputs they `COPY`/`ADD`)
    /// from a custom non-Git task's directory into this context. Reads only.
    pub fn plan_legacy_migration(
        &self,
        legacy_root: &Path,
        agents: &[String],
    ) -> Result<LegacyRecipePlan, DataError> {
        let tree = self.tree()?;
        if std::fs::canonicalize(legacy_root).ok().as_deref() == Some(tree.root()) {
            return Ok(LegacyRecipePlan::Nothing);
        }
        let legacy = RepoTree::open(legacy_root).map_err(tree_error)?;
        let mut candidates = vec![PROJECT_RECIPE.to_string()];
        for agent in agents {
            candidates.push(agent_recipe(agent)?);
        }
        let mut planner = Planner {
            context: &tree,
            legacy: &legacy,
            files: Vec::new(),
            kept: Vec::new(),
            input_files: 0,
            input_bytes: 0,
        };
        let mut found = false;
        for recipe in candidates {
            let bytes = match read_legacy(&legacy, &recipe)? {
                Legacy::Missing => continue,
                Legacy::Refused => return Ok(refuse(&recipe, &recipe, LegacyRefusal::Symlink)),
                Legacy::File(bytes) => bytes,
            };
            found = true;
            match tree.read(&rel(&recipe)?).map_err(tree_error)? {
                RepoEntry::File(_) => {
                    planner.kept.push(recipe);
                    continue;
                }
                RepoEntry::Other(kind) => {
                    return Err(DataError::InvalidPath {
                        path: self.root.join(&recipe),
                        reason: format!("is a {kind}; image recipes must be regular files"),
                    })
                }
                RepoEntry::Missing => {}
            }
            planner.push(
                &recipe,
                LegacyKind::Recipe,
                Some(crate::data::fs::hash::sha256_hex(&bytes)),
            );
            if let Some(refusal) = planner.plan_inputs(&recipe, &bytes)? {
                return Ok(refusal);
            }
        }
        if !found {
            return Ok(LegacyRecipePlan::Nothing);
        }
        Ok(LegacyRecipePlan::Copy {
            files: planner.files,
            kept: planner.kept,
        })
    }

    /// Copy a planned migration, create-if-missing throughout. A refused or
    /// empty plan copies nothing. The custom directory is only read.
    pub fn apply_legacy_migration(
        &self,
        legacy_root: &Path,
        plan: &LegacyRecipePlan,
    ) -> Result<(), DataError> {
        let LegacyRecipePlan::Copy { files, .. } = plan else {
            return Ok(());
        };
        let tree = self.tree()?;
        let legacy = RepoTree::open(legacy_root).map_err(tree_error)?;
        let changed = |rel: &str| {
            DataError::Other(format!(
                "{rel} changed in {} during migration; trigger the task again",
                legacy_root.display()
            ))
        };
        // Inputs and directories first, recipes last. A recipe in the context
        // then means every input planned with it is installed, so a
        // migration interrupted before its recipes is planned again on retry
        // instead of being taken for a task-owned recipe.
        let ordered = files
            .iter()
            .filter(|file| file.kind != LegacyKind::Recipe)
            .chain(files.iter().filter(|file| file.kind == LegacyKind::Recipe));
        for file in ordered {
            let target = rel(&file.rel)?;
            if file.kind == LegacyKind::Directory {
                tree.create_dir_all(&target).map_err(tree_error)?;
                continue;
            }
            let bytes = match read_legacy(&legacy, &file.rel)? {
                Legacy::File(bytes) => bytes,
                _ => return Err(changed(&file.rel)),
            };
            // Only the bytes the plan approved are installed.
            let digest = crate::data::fs::hash::sha256_hex(&bytes);
            if file.digest.as_deref() != Some(digest.as_str()) {
                return Err(changed(&file.rel));
            }
            if tree.create_new(&target, &bytes).map_err(tree_error)? == CreateOutcome::Created {
                copy_mode(&legacy.path_of(&target), &tree.path_of(&target));
                continue;
            }
            // Created by someone else since the plan: accepted only when it
            // holds exactly the approved bytes.
            match tree.read(&target).map_err(tree_error)? {
                RepoEntry::File(existing)
                    if crate::data::fs::hash::sha256_hex(&existing) == digest => {}
                _ => {
                    return Err(DataError::Other(format!(
                        "{} in the task image context {} differs from {} in {}; \
                         nothing more was copied. Remove one of them and trigger the task again.",
                        file.rel,
                        self.root.display(),
                        file.rel,
                        legacy_root.display()
                    )))
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn copy_mode(from: &Path, to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::symlink_metadata(from) {
        let mode = meta.permissions().mode() & 0o777;
        let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn copy_mode(_: &Path, _: &Path) {}

enum Legacy {
    Missing,
    Refused,
    File(Vec<u8>),
}

fn read_legacy(legacy: &RepoTree, path: &str) -> Result<Legacy, DataError> {
    match legacy.read(&rel(path)?) {
        Ok(RepoEntry::Missing) => Ok(Legacy::Missing),
        Ok(RepoEntry::File(bytes)) => Ok(Legacy::File(bytes)),
        Ok(RepoEntry::Other(_)) => Ok(Legacy::Refused),
        Err(error) if error.is_refusal() => Ok(Legacy::Refused),
        Err(error) => Err(tree_error(error)),
    }
}

fn refuse(recipe: &str, input: &str, why: LegacyRefusal) -> LegacyRecipePlan {
    LegacyRecipePlan::Refuse {
        recipe: recipe.into(),
        input: input.into(),
        why,
    }
}

struct Planner<'a> {
    context: &'a RepoTree,
    legacy: &'a RepoTree,
    files: Vec<LegacyCopy>,
    kept: Vec<String>,
    input_files: usize,
    input_bytes: u64,
}

impl Planner<'_> {
    fn push(&mut self, rel: &str, kind: LegacyKind, digest: Option<String>) {
        if !self.files.iter().any(|file| file.rel == rel) {
            self.files.push(LegacyCopy {
                rel: rel.into(),
                kind,
                digest,
            });
        }
    }

    /// Plan every local input `recipe` names. `Some` is a refusal.
    fn plan_inputs(
        &mut self,
        recipe: &str,
        bytes: &[u8],
    ) -> Result<Option<LegacyRecipePlan>, DataError> {
        let sources = resolve_copy_sources(&String::from_utf8_lossy(bytes));
        if sources.is_empty() {
            return Ok(None);
        }
        for ignore in [
            ".dockerignore".to_string(),
            format!("{recipe}.dockerignore"),
        ] {
            if let Some(refusal) = self.plan_file(recipe, &ignore, LegacyKind::DockerIgnore)? {
                return Ok(Some(refusal));
            }
        }
        for source in sources {
            let path = match source {
                CopySource::Path(path) => path,
                CopySource::Unresolved(unresolved) => {
                    let why = match unresolved.reason {
                        UnresolvedReason::Remote => continue,
                        UnresolvedReason::WholeContext => LegacyRefusal::WholeContext,
                        UnresolvedReason::OutsideContext => LegacyRefusal::EscapesDirectory,
                        UnresolvedReason::TooLarge => LegacyRefusal::TooLarge,
                        UnresolvedReason::Glob
                        | UnresolvedReason::Variable
                        | UnresolvedReason::Unreadable => LegacyRefusal::Unresolvable,
                    };
                    return Ok(Some(refuse(recipe, &unresolved.source, why)));
                }
            };
            if let Some(refusal) = self.plan_source(recipe, &path)? {
                return Ok(Some(refusal));
            }
        }
        Ok(None)
    }

    /// A file or a directory walked recursively. A missing source is left
    /// alone: the build fails on it exactly as it did from the old folder.
    fn plan_source(
        &mut self,
        recipe: &str,
        path: &str,
    ) -> Result<Option<LegacyRecipePlan>, DataError> {
        match self.legacy.read(&rel(path)?) {
            Ok(RepoEntry::Missing) => Ok(None),
            Ok(RepoEntry::File(_)) => self.plan_file(recipe, path, LegacyKind::BuildInput),
            Ok(RepoEntry::Other("directory")) => {
                let dir = self.legacy.path_of(&rel(path)?);
                let mut names: Vec<String> = std::fs::read_dir(&dir)
                    .map_err(|e| DataError::io(&dir, e))?
                    .map(|entry| {
                        entry
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .map_err(|e| DataError::io(&dir, e))
                    })
                    .collect::<Result<_, _>>()?;
                names.sort();
                // The directory itself, so an empty one (or one whose files
                // all exist already) is still created. Nothing is skipped:
                // `.git` inside a copied directory is part of what Docker
                // would copy, so it is migrated too.
                if !matches!(
                    self.context.read(&rel(path)?).map_err(tree_error)?,
                    RepoEntry::Other("directory")
                ) {
                    self.push(path, LegacyKind::Directory, None);
                }
                for name in names {
                    if let Some(refusal) = self.plan_source(recipe, &format!("{path}/{name}"))? {
                        return Ok(Some(refusal));
                    }
                }
                Ok(None)
            }
            Ok(RepoEntry::Other(_)) => Ok(Some(refuse(recipe, path, LegacyRefusal::Symlink))),
            Err(error) if error.is_refusal() => {
                Ok(Some(refuse(recipe, path, LegacyRefusal::Symlink)))
            }
            Err(error) => Err(tree_error(error)),
        }
    }

    /// One regular file: copy when the destination is missing, nothing when
    /// it holds the same bytes, refuse when it differs.
    fn plan_file(
        &mut self,
        recipe: &str,
        path: &str,
        kind: LegacyKind,
    ) -> Result<Option<LegacyRecipePlan>, DataError> {
        let source = rel(path)?;
        let size = std::fs::symlink_metadata(self.legacy.path_of(&source))
            .map(|meta| meta.len())
            .unwrap_or(0);
        if size > MAX_LEGACY_INPUT_BYTES {
            return Ok(Some(refuse(recipe, path, LegacyRefusal::TooLarge)));
        }
        let bytes = match read_legacy(self.legacy, path)? {
            Legacy::Missing => return Ok(None),
            Legacy::Refused => return Ok(Some(refuse(recipe, path, LegacyRefusal::Symlink))),
            Legacy::File(bytes) => bytes,
        };
        match self.context.read(&source).map_err(tree_error)? {
            RepoEntry::File(existing) if existing == bytes => return Ok(None),
            RepoEntry::File(_) | RepoEntry::Other(_) => {
                return Ok(Some(refuse(
                    recipe,
                    path,
                    LegacyRefusal::DestinationDiffers,
                )))
            }
            RepoEntry::Missing => {}
        }
        if self.files.iter().any(|file| file.rel == path) {
            return Ok(None);
        }
        self.input_files += 1;
        self.input_bytes += bytes.len() as u64;
        if self.input_files > MAX_LEGACY_INPUT_FILES || self.input_bytes > MAX_LEGACY_INPUT_BYTES {
            return Ok(Some(refuse(recipe, path, LegacyRefusal::TooLarge)));
        }
        self.push(path, kind, Some(crate::data::fs::hash::sha256_hex(&bytes)));
        Ok(None)
    }
}

#[cfg(test)]
#[path = "image_context_tests.rs"]
mod image_context_tests;
