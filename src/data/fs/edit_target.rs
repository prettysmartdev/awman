//! Typed resolution of `awman edit workflow|skill|agent` targets (Layer 0).
//!
//! [`resolve_artifact_target`] turns a user reference (a name or a path) into
//! an [`ArtifactTarget`]: the canonical bundle root, the exact files the edit
//! may touch with their baseline hashes, and whether the bundle lives inside
//! the mounted checkout (edit in place) or must be staged
//! (see [`crate::data::fs::edit_stage`]).
//!
//! Resolution is lookup only. It never creates a directory or file, never
//! falls back from repository scope to global scope, never executes anything
//! and never records an agent approval. Every file is read through the
//! bounded, no-follow [`SafeTree`] reader; symlinks and special files are not
//! followed and are never part of a bundle.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::data::config::agent_definition::{
    is_first_party_name, load_agent_definition_with_scope, validate_agent_file_stem,
};
use crate::data::config::env::EnvSnapshot;
use crate::data::context::ContextRelPath;
use crate::data::error::DataError;
use crate::data::fs::agent_dirs::{AgentDirs, DefinitionScope, AGENT_DEFINITION_EXT};
use crate::data::fs::safe_tree::{SafeTree, SafeTreeError};
use crate::data::fs::skill_dirs::{SkillDirs, LIBRARY_SUBDIR};
use crate::data::fs::skill_library::read_library_meta;
use crate::data::fs::workflow_dirs::WorkflowDirs;
use crate::data::workflow_definition::{detect_format, Workflow, WorkflowFormat};

/// Largest single file an edit bundle may contain.
pub const MAX_ARTIFACT_FILE_BYTES: u64 = 1_048_576; // 64 KiB still applies to agent files via the loader
/// Most regular files a directory bundle (a skill) may contain.
pub const MAX_BUNDLE_FILES: usize = 200;
/// Largest total size of a bundle.
pub const MAX_BUNDLE_BYTES: u64 = 8 * 1_048_576;

/// Directory entries a bundle walk visits before giving up (guards against a
/// huge tree hiding behind a small number of regular files).
const MAX_WALK_ENTRIES: usize = 10_000;
/// The skill entry point.
pub const SKILL_FILE: &str = "SKILL.md";
/// Repo directory `awman new workflow` writes to, searched before `.awman/workflows`.
pub const REPO_ASPEC_WORKFLOWS_DIR: &str = "aspec/workflows";
const WORKFLOW_EXTENSIONS: [&str; 3] = ["toml", "yaml", "yml"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Workflow,
    Skill,
    Agent,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Skill => "skill",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactScope {
    Repo,
    Global,
    /// A skill inside a pulled library (`~/.awman/skills/.library/<slug>/`).
    Library {
        slug: String,
    },
}

impl ArtifactScope {
    /// The agent-definition scope used for loader diagnostics.
    pub fn definition_scope(&self) -> DefinitionScope {
        match self {
            Self::Repo => DefinitionScope::Repo,
            Self::Global | Self::Library { .. } => DefinitionScope::Global,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactFileRole {
    Primary,
    Companion,
    Supporting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFile {
    /// Path relative to `ArtifactTarget::root`, forward slashes, normal components only.
    pub rel: String,
    pub role: ArtifactFileRole,
    /// Baseline content hash and size, read with the bounded safe reader.
    pub sha256: String,
    pub len: u64,
}

#[derive(Debug, Clone)]
pub struct ArtifactTarget {
    pub kind: ArtifactKind,
    /// Workflow file stem, skill directory name (`<lib>/<skill>` for library
    /// skills), or agent name.
    pub name: String,
    pub scope: ArtifactScope,
    /// Canonical bundle root: the skill directory, or the directory holding
    /// the workflow file / agent TOML.
    pub root: PathBuf,
    /// Baseline files, primary first. Only these (plus, for skills, new files
    /// inside `root`) may ever be written.
    pub files: Vec<ArtifactFile>,
    /// `true` for skills: the agent may add files inside the directory.
    pub directory_bundle: bool,
    /// Workflows only; decided from the existing extension and never changed.
    pub format: Option<WorkflowFormat>,
    /// `Some(repo-relative root, forward slashes; "" for the repo root)` when
    /// `root` is inside the mounted checkout → edit in place. `None` → stage.
    pub workspace_rel: Option<String>,
    /// Parse problems in the artifact as found (it may be opened for repair).
    pub initial_problems: Vec<String>,
    /// e.g. the library refresh notice, entries skipped as unsafe.
    pub notices: Vec<String>,
}

impl ArtifactTarget {
    pub fn primary(&self) -> &ArtifactFile {
        self.files
            .iter()
            .find(|f| f.role == ArtifactFileRole::Primary)
            .unwrap_or(&self.files[0])
    }

    pub fn host_path(&self, file: &ArtifactFile) -> PathBuf {
        join_rel(&self.root, &file.rel)
    }

    pub fn edits_in_place(&self) -> bool {
        self.workspace_rel.is_some()
    }

    /// `/workspace/<workspace_rel>/<rel>`; `None` when staged.
    pub fn workspace_container_path(&self, file: &ArtifactFile) -> Option<String> {
        let base = self.workspace_rel.as_deref()?;
        Some(if base.is_empty() {
            format!("/workspace/{}", file.rel)
        } else {
            format!("/workspace/{base}/{}", file.rel)
        })
    }
}

pub struct ArtifactRequest<'a> {
    pub kind: ArtifactKind,
    pub reference: &'a str,
    pub global: bool,
    pub env: &'a EnvSnapshot,
    /// The checkout mounted at `/workspace` (`Session::git_root`).
    pub git_root: &'a Path,
    /// Where repo agent definitions live (`Session::definition_root`).
    pub definition_root: &'a Path,
    /// Base for relative path references (the invocation directory).
    pub cwd: &'a Path,
}

#[derive(Debug, thiserror::Error)]
pub enum EditTargetError {
    #[error("{}", not_found_message(*.kind, .reference, .searched, .hint.as_deref()))]
    NotFound {
        kind: ArtifactKind,
        reference: String,
        searched: Vec<PathBuf>,
        hint: Option<String>,
    },
    #[error("{kind} '{reference}' is ambiguous; it matches:\n{}\nPass one of these paths instead.", bullet_paths(.candidates))]
    Ambiguous {
        kind: ArtifactKind,
        reference: String,
        candidates: Vec<PathBuf>,
    },
    #[error("{}: {reason}", .path.display())]
    ScopeConflict { path: PathBuf, reason: String },
    #[error("{} is outside the editable locations:\n{}", .path.display(), bullet_paths(.allowed))]
    OutsideScope {
        path: PathBuf,
        allowed: Vec<PathBuf>,
    },
    #[error("refusing to edit {}: {reason}", .path.display())]
    Unsafe { path: PathBuf, reason: String },
    #[error("{kind} '{name}' is built into awman and is read-only; {hint}")]
    BuiltinReadOnly {
        kind: ArtifactKind,
        name: String,
        hint: String,
    },
    #[error("invalid {kind} reference '{reference}': {reason}")]
    InvalidReference {
        kind: ArtifactKind,
        reference: String,
        reason: String,
    },
    #[error("{} is too large to edit (limit {limit})", .path.display())]
    TooLarge { path: PathBuf, limit: u64 },
    #[error(transparent)]
    Data(#[from] DataError),
}

fn bullet_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn not_found_message(
    kind: ArtifactKind,
    reference: &str,
    searched: &[PathBuf],
    hint: Option<&str>,
) -> String {
    let mut out = format!("{kind} '{reference}' not found");
    if !searched.is_empty() {
        out.push_str("; searched:\n");
        out.push_str(&bullet_paths(searched));
    }
    if let Some(hint) = hint {
        out.push('\n');
        out.push_str(hint);
    }
    out
}

// ───────────────────────────── resolution ─────────────────────────────

/// Resolve `request.reference` to a typed, validated-for-safety target.
/// Missing or unresolvable targets are errors; nothing is ever created.
pub fn resolve_artifact_target(
    request: &ArtifactRequest<'_>,
) -> Result<ArtifactTarget, EditTargetError> {
    let reference = request.reference.trim();
    if reference.is_empty() || reference.contains('\0') {
        return Err(invalid(request.kind, reference, "the reference is empty"));
    }
    let ctx = Ctx::new(request)?;
    let path = if is_path_reference(request.kind, reference, request.global) {
        ctx.expand_path(reference)?
    } else {
        ctx.lookup_name(reference)?
    };
    ctx.resolve_path(&path)
}

/// A reference is a path when it contains a separator, starts with `.` or
/// `~`, is absolute, or ends in a known extension. Exception: with
/// `--global`, a skill `<library>/<skill>` (two safe segments) is a name.
fn is_path_reference(kind: ArtifactKind, reference: &str, global: bool) -> bool {
    if kind == ArtifactKind::Skill && global && library_skill_name(reference).is_some() {
        return false;
    }
    let lower = reference.to_ascii_lowercase();
    reference.contains('/')
        || reference.contains('\\')
        || reference.starts_with('.')
        || reference.starts_with('~')
        || Path::new(reference).is_absolute()
        || [".toml", ".yaml", ".yml"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        || reference == SKILL_FILE
        || reference.ends_with("/SKILL.md")
}

fn library_skill_name(reference: &str) -> Option<(&str, &str)> {
    let (library, skill) = reference.split_once('/')?;
    (safe_segment(library) && safe_segment(skill) && !skill.contains('.')).then_some(())?;
    Some((library, skill))
}

/// One plain path segment: non-empty, no separators, not `.`/`..`, no
/// leading dot (hidden entries such as `.library` are never names).
fn safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && !segment.contains(['/', '\\', '\0', ':'])
        && matches!(
            Path::new(segment)
                .components()
                .collect::<Vec<_>>()
                .as_slice(),
            [Component::Normal(_)]
        )
}

fn invalid(kind: ArtifactKind, reference: &str, reason: impl Into<String>) -> EditTargetError {
    EditTargetError::InvalidReference {
        kind,
        reference: reference.to_string(),
        reason: reason.into(),
    }
}

fn unsafe_path(path: &Path, reason: impl Into<String>) -> EditTargetError {
    EditTargetError::Unsafe {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootScope {
    Repo,
    Global,
}

/// One location an artifact of the requested kind may live in.
struct AllowedRoot {
    lexical: PathBuf,
    scope: RootScope,
    /// Managed directories that must be real directories, never symlinks.
    guards: Vec<PathBuf>,
}

/// A path located inside an allowed root.
struct Located {
    /// Canonical allowed root.
    root: PathBuf,
    /// Normal components below `root`.
    rel: Vec<String>,
    scope: RootScope,
}

impl Located {
    fn path(&self) -> PathBuf {
        let mut p = self.root.clone();
        p.extend(&self.rel);
        p
    }
}

struct Ctx<'a> {
    req: &'a ArtifactRequest<'a>,
    git_root: PathBuf,
    workflows: WorkflowDirs,
    skills: SkillDirs,
    agents: AgentDirs,
}

impl<'a> Ctx<'a> {
    fn new(req: &'a ArtifactRequest<'a>) -> Result<Self, EditTargetError> {
        let git_root =
            std::fs::canonicalize(req.git_root).map_err(|e| DataError::io(req.git_root, e))?;
        Ok(Self {
            req,
            workflows: WorkflowDirs::from_env(req.env, Some(git_root.clone()))?,
            skills: SkillDirs::from_env(req.env, Some(git_root.clone()))?,
            agents: AgentDirs::from_env(req.env, Some(req.definition_root.to_path_buf()))?,
            git_root,
        })
    }

    fn kind(&self) -> ArtifactKind {
        self.req.kind
    }

    /// Allowed roots for the kind, most specific (global) first so a global
    /// home that happens to live inside the checkout is still recognised.
    fn allowed_roots(&self) -> Vec<AllowedRoot> {
        let global = |lexical: PathBuf| AllowedRoot {
            guards: vec![lexical.clone()],
            lexical,
            scope: RootScope::Global,
        };
        let repo = AllowedRoot {
            lexical: self.git_root.clone(),
            scope: RootScope::Repo,
            guards: Vec::new(),
        };
        match self.kind() {
            ArtifactKind::Workflow => vec![global(self.workflows.global_dir()), repo],
            ArtifactKind::Skill => vec![global(self.skills.global_dir()), repo],
            ArtifactKind::Agent => {
                let mut roots = vec![global(self.agents.global_dir())];
                if let Some(dir) = self.agents.repo_dir() {
                    roots.push(AllowedRoot {
                        guards: vec![self.req.definition_root.join(".awman"), dir.clone()],
                        lexical: dir,
                        scope: RootScope::Repo,
                    });
                }
                roots
            }
        }
    }

    // ── paths ──

    fn expand_path(&self, reference: &str) -> Result<PathBuf, EditTargetError> {
        let raw = if reference == "~" || reference.starts_with("~/") {
            let home = self.req.env.home_dir().ok_or_else(|| {
                invalid(
                    self.kind(),
                    reference,
                    "HOME is not set, so '~' cannot be expanded",
                )
            })?;
            home.join(reference.trim_start_matches('~').trim_start_matches('/'))
        } else if reference.starts_with('~') {
            return Err(invalid(
                self.kind(),
                reference,
                "only '~/' is supported for home-relative paths",
            ));
        } else {
            let p = PathBuf::from(reference);
            if p.is_absolute() {
                p
            } else {
                self.req.cwd.join(p)
            }
        };
        Ok(normalize_lexically(&raw))
    }

    /// Find `path` inside one allowed root and prove no component below that
    /// root is a symlink and the canonical form stays inside it.
    fn locate(&self, path: &Path) -> Result<Located, EditTargetError> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EditTargetError::NotFound {
                    kind: self.kind(),
                    reference: self.req.reference.to_string(),
                    searched: vec![path.to_path_buf()],
                    hint: None,
                })
            }
            Err(e) => return Err(DataError::io(path, e).into()),
        };
        if meta.file_type().is_symlink() {
            return Err(unsafe_path(path, "it is a symlink"));
        }
        let roots = self.allowed_roots();
        let canonical_roots: Vec<Option<PathBuf>> = roots
            .iter()
            .map(|r| std::fs::canonicalize(&r.lexical).ok())
            .collect();
        // Lexical match first so a symlink below the root is detected.
        for (root, canonical) in roots.iter().zip(&canonical_roots) {
            let Some(canonical) = canonical else { continue };
            let rel = path
                .strip_prefix(&root.lexical)
                .or_else(|_| path.strip_prefix(canonical));
            if let Ok(rel) = rel {
                let rel = rel_components(rel).ok_or_else(|| {
                    unsafe_path(path, "the path contains an unsupported component")
                })?;
                return self.confirm(root, canonical, rel, path);
            }
        }
        // Otherwise the path was reached through a link outside every root:
        // judge it by where it really is.
        let resolved = std::fs::canonicalize(path).map_err(|e| DataError::io(path, e))?;
        for (root, canonical) in roots.iter().zip(&canonical_roots) {
            let Some(canonical) = canonical else { continue };
            if let Ok(rel) = resolved.strip_prefix(canonical) {
                let rel = rel_components(rel).ok_or_else(|| {
                    unsafe_path(path, "the path contains an unsupported component")
                })?;
                return self.confirm(root, canonical, rel, path);
            }
        }
        Err(EditTargetError::OutsideScope {
            path: path.to_path_buf(),
            allowed: roots.into_iter().map(|r| r.lexical).collect(),
        })
    }

    fn confirm(
        &self,
        allowed: &AllowedRoot,
        root: &Path,
        rel: Vec<String>,
        shown: &Path,
    ) -> Result<Located, EditTargetError> {
        let scope = allowed.scope;
        for guard in &allowed.guards {
            if std::fs::symlink_metadata(guard).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(unsafe_path(
                    shown,
                    format!(
                        "{} is a symlink; managed directories must be real directories",
                        guard.display()
                    ),
                ));
            }
        }
        let mut cursor = root.to_path_buf();
        for part in &rel {
            cursor.push(part);
            let meta = std::fs::symlink_metadata(&cursor).map_err(|e| DataError::io(&cursor, e))?;
            if meta.file_type().is_symlink() {
                return Err(unsafe_path(
                    shown,
                    format!("{} is a symlink", cursor.display()),
                ));
            }
        }
        let canonical = std::fs::canonicalize(&cursor).map_err(|e| DataError::io(&cursor, e))?;
        if !canonical.starts_with(root) {
            return Err(unsafe_path(
                shown,
                "it resolves outside its allowed location",
            ));
        }
        if rel.iter().any(|p| p == ".git") {
            return Err(unsafe_path(shown, "git metadata is never editable"));
        }
        if scope == RootScope::Repo && self.req.global {
            return Err(EditTargetError::ScopeConflict {
                path: shown.to_path_buf(),
                reason: format!(
                    "this is a repository {}; drop --global to edit it",
                    self.kind()
                ),
            });
        }
        Ok(Located {
            root: root.to_path_buf(),
            rel,
            scope,
        })
    }

    // ── names ──

    fn lookup_name(&self, name: &str) -> Result<PathBuf, EditTargetError> {
        match self.kind() {
            ArtifactKind::Workflow => self.lookup_workflow(name),
            ArtifactKind::Skill => self.lookup_skill(name),
            ArtifactKind::Agent => self.lookup_agent(name),
        }
    }

    fn not_found(&self, searched: Vec<PathBuf>, other_scope_has_it: bool) -> EditTargetError {
        let hint = other_scope_has_it.then(|| {
            if self.req.global {
                format!(
                    "a repository {} with this name exists; drop --global to edit it",
                    self.kind()
                )
            } else {
                format!(
                    "a global {} with this name exists; pass --global to edit it",
                    self.kind()
                )
            }
        });
        EditTargetError::NotFound {
            kind: self.kind(),
            reference: self.req.reference.to_string(),
            searched,
            hint,
        }
    }

    fn workflow_dirs(&self, global: bool) -> Vec<PathBuf> {
        if global {
            vec![self.workflows.global_dir()]
        } else {
            vec![
                self.git_root.join(REPO_ASPEC_WORKFLOWS_DIR),
                WorkflowDirs::repo_dir_for(&self.git_root),
            ]
        }
    }

    fn workflow_candidates(&self, name: &str, global: bool) -> Vec<PathBuf> {
        let stash_dir = WorkflowDirs::repo_dir_for(&self.git_root);
        let mut found = Vec::new();
        for dir in self.workflow_dirs(global) {
            if !global && dir == stash_dir && name.starts_with("dynamic-") {
                continue; // machine-written stash files are not named workflows
            }
            for ext in WORKFLOW_EXTENSIONS {
                let path = dir.join(format!("{name}.{ext}"));
                if std::fs::symlink_metadata(&path).is_ok() {
                    found.push(path);
                }
            }
        }
        found
    }

    fn lookup_workflow(&self, name: &str) -> Result<PathBuf, EditTargetError> {
        if !safe_segment(name) {
            return Err(invalid(self.kind(), name, "not a valid workflow name"));
        }
        let mut found = self.workflow_candidates(name, self.req.global);
        match found.len() {
            0 => {
                let other = !self.workflow_candidates(name, !self.req.global).is_empty();
                Err(self.not_found(self.workflow_dirs(self.req.global), other))
            }
            1 => Ok(found.remove(0)),
            _ => Err(EditTargetError::Ambiguous {
                kind: self.kind(),
                reference: name.to_string(),
                candidates: found,
            }),
        }
    }

    fn lookup_skill(&self, name: &str) -> Result<PathBuf, EditTargetError> {
        if self.req.global {
            if let Some((library, skill)) = library_skill_name(name) {
                return self.lookup_library_skill(library, skill);
            }
        }
        if !safe_segment(name) {
            return Err(invalid(self.kind(), name, "not a valid skill name"));
        }
        let repo = SkillDirs::repo_dir_for(&self.git_root).join(name);
        let global = self.skills.global_dir().join(name);
        let is_skill = |dir: &Path| dir.join(SKILL_FILE).exists();
        if !self.req.global {
            if std::fs::symlink_metadata(&repo).is_ok() {
                return Ok(repo);
            }
            return Err(self.not_found(vec![repo], is_skill(&global)));
        }
        if std::fs::symlink_metadata(&global).is_ok() {
            return Ok(global);
        }
        if std::fs::symlink_metadata(self.skills.library_dir(name)).is_ok() {
            return Err(invalid(
                self.kind(),
                name,
                format!("'{name}' names a pulled library; select one skill as `{name}/<skill>`"),
            ));
        }
        Err(self.not_found(vec![global], is_skill(&repo)))
    }

    fn lookup_library_skill(&self, library: &str, skill: &str) -> Result<PathBuf, EditTargetError> {
        let library_dir = self.skills.library_dir(library);
        match std::fs::symlink_metadata(&library_dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(unsafe_path(
                    &library_dir,
                    "the library directory is a symlink",
                ))
            }
            Ok(meta) if meta.is_dir() => {}
            _ => {
                return Err(self.not_found(vec![library_dir], false));
            }
        }
        let subdir = library_subdir(&library_dir)?;
        let mut dir = library_dir;
        dir.extend(&subdir);
        dir.push(skill);
        if std::fs::symlink_metadata(&dir).is_err() {
            return Err(self.not_found(vec![dir], false));
        }
        Ok(dir)
    }

    fn lookup_agent(&self, name: &str) -> Result<PathBuf, EditTargetError> {
        validate_agent_file_stem(name).map_err(|reason| invalid(self.kind(), name, reason))?;
        if is_first_party_name(name) {
            return Err(builtin_agent(name));
        }
        let (scope, other) = if self.req.global {
            (DefinitionScope::Global, DefinitionScope::Repo)
        } else {
            (DefinitionScope::Repo, DefinitionScope::Global)
        };
        let Some(path) = self.agents.definition_path(scope, name) else {
            return Err(self.not_found(Vec::new(), false));
        };
        if std::fs::symlink_metadata(&path).is_ok() {
            return Ok(path);
        }
        let other_has = self
            .agents
            .definition_path(other, name)
            .is_some_and(|p| p.is_file());
        Err(self.not_found(vec![path], other_has))
    }

    // ── per-kind bundles ──

    fn resolve_path(&self, path: &Path) -> Result<ArtifactTarget, EditTargetError> {
        let located = self.locate(path)?;
        match self.kind() {
            ArtifactKind::Workflow => self.workflow_target(located, path),
            ArtifactKind::Skill => self.skill_target(located, path),
            ArtifactKind::Agent => self.agent_target(located, path),
        }
    }

    fn base_target(
        &self,
        name: String,
        scope: ArtifactScope,
        root: PathBuf,
        files: Vec<ArtifactFile>,
    ) -> ArtifactTarget {
        let workspace_rel = root
            .strip_prefix(&self.git_root)
            .ok()
            .and_then(|rel| rel_components(rel).map(|parts| parts.join("/")));
        ArtifactTarget {
            kind: self.kind(),
            name,
            scope,
            root,
            files,
            directory_bundle: false,
            format: None,
            workspace_rel,
            initial_problems: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn workflow_target(
        &self,
        located: Located,
        shown: &Path,
    ) -> Result<ArtifactTarget, EditTargetError> {
        let full = located.path();
        if !full.is_file() {
            return Err(invalid(
                self.kind(),
                self.req.reference,
                "a workflow must be a file",
            ));
        }
        let format = detect_format(&full)
            .map_err(|e| invalid(self.kind(), self.req.reference, e.to_string()))?;
        let file_name = located.rel.last().cloned().unwrap_or_default();
        let root = full.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = Path::new(&file_name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let primary = baseline_file(
            self.kind(),
            &root,
            &file_name,
            ArtifactFileRole::Primary,
            shown,
        )?;
        let mut target = self.base_target(name, scope_of(located.scope), root, vec![primary]);
        target.format = Some(format);
        target.initial_problems = validate_live(&target);
        Ok(target)
    }

    fn skill_target(
        &self,
        located: Located,
        shown: &Path,
    ) -> Result<ArtifactTarget, EditTargetError> {
        let mut rel = located.rel.clone();
        let full = located.path();
        let meta = std::fs::symlink_metadata(&full).map_err(|e| DataError::io(&full, e))?;
        if meta.is_file() {
            if rel.last().map(String::as_str) != Some(SKILL_FILE) {
                return Err(invalid(
                    self.kind(),
                    self.req.reference,
                    "select a skill directory or its SKILL.md",
                ));
            }
            rel.pop();
        } else if !meta.is_dir() {
            return Err(unsafe_path(shown, "not a regular file or directory"));
        }
        let skill_md = {
            let mut p = located.root.clone();
            p.extend(&rel);
            p.join(SKILL_FILE)
        };
        match std::fs::symlink_metadata(&skill_md) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(unsafe_path(&skill_md, "SKILL.md is a symlink"))
            }
            Ok(m) if m.is_file() => {}
            _ => {
                return Err(invalid(
                    self.kind(),
                    self.req.reference,
                    format!("{} is not a skill directory (no SKILL.md)", shown.display()),
                ))
            }
        }
        if rel.is_empty() {
            return Err(invalid(
                self.kind(),
                self.req.reference,
                "a whole repository or skills directory cannot be edited as one skill",
            ));
        }
        let (name, scope, notices) = match located.scope {
            RootScope::Repo => (
                rel.last().cloned().unwrap_or_default(),
                ArtifactScope::Repo,
                vec![],
            ),
            RootScope::Global if rel[0] == LIBRARY_SUBDIR => {
                self.library_skill_identity(&located.root, &rel, shown)?
            }
            RootScope::Global => (
                rel.last().cloned().unwrap_or_default(),
                ArtifactScope::Global,
                vec![],
            ),
        };
        let mut root = located.root.clone();
        root.extend(&rel);
        let (files, mut skipped) = skill_bundle(&root)?;
        let mut target = self.base_target(name, scope, root, files);
        target.directory_bundle = true;
        target.notices = notices;
        target.notices.append(&mut skipped);
        target.initial_problems = validate_live(&target);
        Ok(target)
    }

    /// `rel` = [".library", slug, <subdir...>, skill] exactly.
    fn library_skill_identity(
        &self,
        global_skills: &Path,
        rel: &[String],
        shown: &Path,
    ) -> Result<(String, ArtifactScope, Vec<String>), EditTargetError> {
        let Some(slug) = rel.get(1) else {
            return Err(invalid(
                self.kind(),
                self.req.reference,
                "select one skill inside a library",
            ));
        };
        let library_dir = global_skills.join(LIBRARY_SUBDIR).join(slug);
        let subdir = library_subdir(&library_dir)?;
        let below = &rel[2..];
        let skill = match below.split_last() {
            Some((skill, parents)) if parents == subdir.as_slice() => skill.clone(),
            _ => {
                return Err(EditTargetError::OutsideScope {
                    path: shown.to_path_buf(),
                    allowed: vec![library_dir.join(subdir.join("/")).join("<skill>")],
                })
            }
        };
        let notice = format!(
            "`{slug}` is a pulled library; `awman new skill --pull`/`--pull-all` may replace local changes"
        );
        Ok((
            format!("{slug}/{skill}"),
            ArtifactScope::Library { slug: slug.clone() },
            vec![notice],
        ))
    }

    fn agent_target(
        &self,
        located: Located,
        shown: &Path,
    ) -> Result<ArtifactTarget, EditTargetError> {
        let [file_name] = located.rel.as_slice() else {
            return Err(EditTargetError::OutsideScope {
                path: shown.to_path_buf(),
                allowed: self
                    .allowed_roots()
                    .into_iter()
                    .map(|r| r.lexical)
                    .collect(),
            });
        };
        let stem = file_name
            .strip_suffix(&format!(".{AGENT_DEFINITION_EXT}"))
            .ok_or_else(|| {
                invalid(
                    self.kind(),
                    self.req.reference,
                    "an agent definition is a .toml file",
                )
            })?;
        validate_agent_file_stem(stem).map_err(|r| invalid(self.kind(), self.req.reference, r))?;
        if is_first_party_name(stem) {
            return Err(builtin_agent(stem));
        }
        let root = located.root.clone();
        if !root.join(file_name).is_file() {
            return Err(unsafe_path(shown, "not a regular file"));
        }
        let primary = baseline_file(
            self.kind(),
            &root,
            file_name,
            ArtifactFileRole::Primary,
            shown,
        )?;
        let toml_bytes = read_bounded(&root, file_name, MAX_ARTIFACT_FILE_BYTES)
            .map(|(bytes, _)| bytes)
            .unwrap_or_default();
        let mut files = vec![primary];
        let mut problems = Vec::new();
        if let Some(reference) = probe_dockerfile_ref(&toml_bytes) {
            match companion_state(&root, &reference) {
                Companion::Present => files.push(baseline_file(
                    ArtifactKind::Agent,
                    &root,
                    &reference,
                    ArtifactFileRole::Companion,
                    shown,
                )?),
                Companion::Missing => problems.push(format!(
                    "image.dockerfile: {} does not exist",
                    join_rel(&root, &reference).display()
                )),
                Companion::Unsafe(reason) => {
                    return Err(unsafe_path(&root.join(&reference), reason))
                }
            }
        }
        let mut target = self.base_target(stem.to_string(), scope_of(located.scope), root, files);
        let validation = validate_live(&target);
        // The loader reports a missing Dockerfile too; keep only its wording.
        if validation.iter().any(|v| v.contains("image.dockerfile")) {
            problems.clear();
        }
        problems.extend(validation);
        target.initial_problems = problems;
        Ok(target)
    }
}

fn scope_of(scope: RootScope) -> ArtifactScope {
    match scope {
        RootScope::Repo => ArtifactScope::Repo,
        RootScope::Global => ArtifactScope::Global,
    }
}

fn builtin_agent(name: &str) -> EditTargetError {
    EditTargetError::BuiltinReadOnly {
        kind: ArtifactKind::Agent,
        name: name.to_string(),
        hint: "first-party agents are built into awman; create a custom definition with `awman new agent <name>`".into(),
    }
}

/// Read and validate a library's `.awman.json` `subdir` as normal components.
fn library_subdir(library_dir: &Path) -> Result<Vec<String>, EditTargetError> {
    let meta = read_library_meta(library_dir)?;
    let parts = rel_components(Path::new(&meta.subdir)).filter(|p| !p.is_empty());
    parts.ok_or_else(|| {
        unsafe_path(
            library_dir,
            format!(
                "library subdir '{}' must be a relative path inside the library",
                meta.subdir
            ),
        )
    })
}

/// Normal components as UTF-8 strings; `None` for `..`, roots, prefixes or
/// non-UTF-8 names. `.` components are dropped.
fn rel_components(rel: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(part) => out.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Resolve `.` and `..` without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(crate) fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    p.extend(rel.split('/'));
    p
}

/// `image.dockerfile` read leniently so a broken definition can still be
/// opened for repair. Never executes or validates anything else.
pub(crate) fn probe_dockerfile_ref(toml_bytes: &[u8]) -> Option<String> {
    let source = std::str::from_utf8(toml_bytes).ok()?;
    let table: toml::Table = toml::from_str(source).ok()?;
    table
        .get("image")?
        .as_table()?
        .get("dockerfile")?
        .as_str()
        .map(str::to_owned)
}

/// The loader's own containment rule for `image.dockerfile`.
pub(crate) fn normal_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\0', '\\'])
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && ContextRelPath::parse(value).is_ok()
}

pub(crate) enum Companion {
    Present,
    Missing,
    Unsafe(String),
}

/// Where an agent's companion reference points, judged without following links.
pub(crate) fn companion_state(root: &Path, reference: &str) -> Companion {
    if !normal_relative(reference) {
        return Companion::Unsafe(format!(
            "image.dockerfile '{reference}' must be a normal relative path inside {}",
            root.display()
        ));
    }
    let mut cursor = root.to_path_buf();
    let parts: Vec<&str> = reference.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        cursor.push(part);
        match std::fs::symlink_metadata(&cursor) {
            Ok(m) if m.file_type().is_symlink() => {
                return Companion::Unsafe(format!("{} is a symlink", cursor.display()))
            }
            Ok(m) if i + 1 < parts.len() && !m.is_dir() => return Companion::Missing,
            Ok(m) if i + 1 == parts.len() && !m.is_file() => {
                return Companion::Unsafe(format!("{} is not a regular file", cursor.display()))
            }
            Ok(_) => {}
            Err(_) => return Companion::Missing,
        }
    }
    Companion::Present
}

// ───────────────────────────── bounded reads ─────────────────────────────

#[derive(Debug)]
pub(crate) enum ReadFailure {
    Missing,
    TooLarge,
    Unsafe(String),
}

/// Read `<root>/<rel>` with the no-follow bounded reader. `root` must be
/// canonical: it is never resolved here, so a swapped link fails closed.
pub(crate) fn read_bounded(
    root: &Path,
    rel: &str,
    max: u64,
) -> Result<(Vec<u8>, String), ReadFailure> {
    let rel_path = ContextRelPath::parse(rel)
        .map_err(|e| ReadFailure::Unsafe(format!("unsupported file name: {e}")))?;
    match SafeTree::new(root).read(&rel_path, max) {
        Ok((bytes, meta)) => Ok((bytes, meta.sha256)),
        Err(SafeTreeError::TooLarge { .. }) => Err(ReadFailure::TooLarge),
        Err(_) => match std::fs::symlink_metadata(join_rel(root, rel)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ReadFailure::Missing),
            Ok(m) if m.file_type().is_symlink() => Err(ReadFailure::Unsafe("is a symlink".into())),
            Ok(m) if !m.is_file() => Err(ReadFailure::Unsafe("is not a regular file".into())),
            _ => Err(ReadFailure::Unsafe(
                "could not be read safely (a symlinked parent, or it changed while being read)"
                    .into(),
            )),
        },
    }
}

fn baseline_file(
    kind: ArtifactKind,
    root: &Path,
    rel: &str,
    role: ArtifactFileRole,
    shown: &Path,
) -> Result<ArtifactFile, EditTargetError> {
    let path = join_rel(root, rel);
    match read_bounded(root, rel, MAX_ARTIFACT_FILE_BYTES) {
        Ok((bytes, sha256)) => Ok(ArtifactFile {
            rel: rel.to_string(),
            role,
            sha256,
            len: bytes.len() as u64,
        }),
        Err(ReadFailure::TooLarge) => Err(EditTargetError::TooLarge {
            path,
            limit: MAX_ARTIFACT_FILE_BYTES,
        }),
        Err(ReadFailure::Missing) => Err(EditTargetError::NotFound {
            kind,
            reference: shown.display().to_string(),
            searched: vec![path],
            hint: None,
        }),
        Err(ReadFailure::Unsafe(reason)) => Err(unsafe_path(&path, reason)),
    }
}

/// One entry found while walking a bundle directory without following links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WalkEntry {
    File {
        rel: String,
        len: u64,
    },
    /// Symlink, socket, FIFO, device, a `.git` directory, or a name awman
    /// cannot represent portably. `rel` is display-only.
    Skipped {
        rel: String,
        reason: &'static str,
    },
}

/// Walk `root` (a real directory) depth-first, sorted, never following
/// symlinks. Fails with `TooLarge` past [`MAX_WALK_ENTRIES`].
pub(crate) fn walk_bundle(root: &Path) -> Result<Vec<WalkEntry>, EditTargetError> {
    let mut out = Vec::new();
    let mut seen = 0usize;
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| DataError::io(&dir, e))?
            .collect::<Result<_, _>>()
            .map_err(|e| DataError::io(&dir, e))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            seen += 1;
            if seen > MAX_WALK_ENTRIES {
                return Err(EditTargetError::TooLarge {
                    path: root.to_path_buf(),
                    limit: MAX_BUNDLE_FILES as u64,
                });
            }
            let raw_name = entry.file_name();
            let name = raw_name.to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let skip = |reason| WalkEntry::Skipped {
                rel: rel.clone(),
                reason,
            };
            if raw_name.to_str().is_none() || ContextRelPath::parse(&rel).is_err() {
                out.push(skip("the name is not portable"));
                continue;
            }
            let meta = match std::fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(DataError::io(entry.path(), e).into()),
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                out.push(skip("it is a symlink"));
            } else if ft.is_dir() {
                if name == ".git" {
                    out.push(skip("git metadata is never part of a bundle"));
                } else if depth + 1 >= ContextRelPath::MAX_DEPTH {
                    out.push(skip("it is nested too deeply"));
                } else {
                    stack.push((entry.path(), rel, depth + 1));
                }
            } else if ft.is_file() {
                out.push(WalkEntry::File {
                    rel,
                    len: meta.len(),
                });
            } else {
                out.push(skip("it is not a regular file"));
            }
        }
    }
    out.sort_by(|a, b| entry_rel(a).cmp(entry_rel(b)));
    Ok(out)
}

fn entry_rel(e: &WalkEntry) -> &str {
    match e {
        WalkEntry::File { rel, .. } | WalkEntry::Skipped { rel, .. } => rel,
    }
}

/// Enforce the bundle limits on a list of `(rel, len)`.
pub(crate) fn check_bundle_limits(
    root: &Path,
    files: &[(String, u64)],
) -> Result<(), EditTargetError> {
    if files.len() > MAX_BUNDLE_FILES {
        return Err(EditTargetError::TooLarge {
            path: root.to_path_buf(),
            limit: MAX_BUNDLE_FILES as u64,
        });
    }
    if let Some((rel, _)) = files.iter().find(|(_, len)| *len > MAX_ARTIFACT_FILE_BYTES) {
        return Err(EditTargetError::TooLarge {
            path: join_rel(root, rel),
            limit: MAX_ARTIFACT_FILE_BYTES,
        });
    }
    if files.iter().map(|(_, len)| *len).sum::<u64>() > MAX_BUNDLE_BYTES {
        return Err(EditTargetError::TooLarge {
            path: root.to_path_buf(),
            limit: MAX_BUNDLE_BYTES,
        });
    }
    Ok(())
}

/// Every regular file in the skill directory; `SKILL.md` first. Skipped
/// entries come back as notices.
fn skill_bundle(root: &Path) -> Result<(Vec<ArtifactFile>, Vec<String>), EditTargetError> {
    let mut regular = Vec::new();
    let mut notices = Vec::new();
    for entry in walk_bundle(root)? {
        match entry {
            WalkEntry::File { rel, len } => regular.push((rel, len)),
            WalkEntry::Skipped { rel, reason } => notices.push(format!(
                "{} is not part of the editable bundle: {reason}",
                join_rel(root, &rel).display()
            )),
        }
    }
    check_bundle_limits(root, &regular)?;
    regular.sort_by_key(|(rel, _)| (rel != SKILL_FILE, rel.clone()));
    let mut files = Vec::new();
    for (rel, _) in regular {
        let role = if rel == SKILL_FILE {
            ArtifactFileRole::Primary
        } else {
            ArtifactFileRole::Supporting
        };
        files.push(baseline_file(ArtifactKind::Skill, root, &rel, role, root)?);
    }
    Ok((files, notices))
}

// ───────────────────────────── validation ─────────────────────────────

/// Parse-only validation of a bundle laid out under `root`. Never executes
/// anything, never records approval. The parsers follow paths normally, so
/// `root` must be a canonical private directory nobody else can write (the
/// validation copy made from a bounded no-follow snapshot); it is never
/// re-resolved here. Returns the problems found; empty means valid.
pub fn validate_artifact(
    kind: ArtifactKind,
    scope: &ArtifactScope,
    root: &Path,
    primary_rel: &str,
    format: Option<WorkflowFormat>,
) -> Vec<String> {
    let primary_path = join_rel(root, primary_rel);
    let shown = primary_path.display().to_string();
    match kind {
        ArtifactKind::Agent => {
            match load_agent_definition_with_scope(&primary_path, scope.definition_scope()) {
                Ok(_) => Vec::new(),
                Err(invalid) => invalid
                    .problems
                    .into_iter()
                    .map(|p| format!("{shown}: {p}"))
                    .collect(),
            }
        }
        ArtifactKind::Workflow | ArtifactKind::Skill => {
            let content = match read_bounded(root, primary_rel, MAX_ARTIFACT_FILE_BYTES) {
                Ok((bytes, _)) => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => return vec![format!("{shown}: is not valid UTF-8")],
                },
                Err(ReadFailure::Missing) => return vec![format!("{shown}: is missing")],
                Err(ReadFailure::TooLarge) => {
                    return vec![format!("{shown}: exceeds {MAX_ARTIFACT_FILE_BYTES} bytes")]
                }
                Err(ReadFailure::Unsafe(reason)) => return vec![format!("{shown}: {reason}")],
            };
            if kind == ArtifactKind::Workflow {
                let format = match format
                    .map(Ok)
                    .unwrap_or_else(|| detect_format(&primary_path))
                {
                    Ok(f) => f,
                    Err(e) => return vec![format!("{shown}: {e}")],
                };
                match Workflow::parse(&content, format) {
                    Ok(_) => Vec::new(),
                    Err(e) => vec![format!("{shown}: {e}")],
                }
            } else {
                let mut problems = skill_md_problems(&content)
                    .into_iter()
                    .map(|p| format!("{shown}: {p}"))
                    .collect::<Vec<_>>();
                match walk_bundle(root) {
                    Ok(entries) => {
                        let files: Vec<(String, u64)> = entries
                            .into_iter()
                            .filter_map(|e| match e {
                                WalkEntry::File { rel, len } => Some((rel, len)),
                                WalkEntry::Skipped { .. } => None,
                            })
                            .collect();
                        if let Err(e) = check_bundle_limits(root, &files) {
                            problems.push(e.to_string());
                        }
                    }
                    Err(e) => problems.push(e.to_string()),
                }
                problems
            }
        }
    }
}

/// `SKILL.md` must not be blank; a leading `---` frontmatter block must be
/// closed and parse as a YAML mapping.
fn skill_md_problems(content: &str) -> Vec<String> {
    if content.trim().is_empty() {
        return vec!["SKILL.md is empty".into()];
    }
    let mut lines = content.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Vec::new();
    }
    let mut block = Vec::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        block.push(line);
    }
    if !closed {
        return vec!["the frontmatter block starting with '---' is never closed".into()];
    }
    let yaml = block.join("\n");
    if yaml.trim().is_empty() {
        return Vec::new();
    }
    match serde_yaml::from_str::<serde_yaml::Value>(&yaml) {
        Ok(serde_yaml::Value::Mapping(_)) => Vec::new(),
        Ok(_) => vec!["the frontmatter is not a YAML mapping".into()],
        Err(e) => vec![format!("the frontmatter is not valid YAML: {e}")],
    }
}

// ───────────────────────────── in-place check ─────────────────────────────

pub struct InPlaceReport {
    pub changed: Vec<PathBuf>,
    pub problems: Vec<String>,
}

/// After an in-place session: re-read the bundle, list files whose hash differs
/// from the baseline (and new files for a skill), and validate. Reads only.
///
/// Validation runs on a private copy of what the bounded no-follow reader
/// returned, never on the live (agent-writable) tree: the parsers follow
/// paths normally, so a file swapped for a symlink must never reach them.
/// When any member cannot be read safely, validation is skipped and only the
/// unsafe-read problems are reported.
pub fn check_in_place(target: &ArtifactTarget) -> Result<InPlaceReport, EditTargetError> {
    let mut changed = Vec::new();
    let mut problems = Vec::new();
    let mut snapshot: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut unreadable = false;
    let baseline: BTreeSet<&str> = target.files.iter().map(|f| f.rel.as_str()).collect();
    for file in &target.files {
        let path = target.host_path(file);
        match read_bounded(&target.root, &file.rel, MAX_ARTIFACT_FILE_BYTES) {
            Ok((bytes, sha)) => {
                if sha != file.sha256 {
                    changed.push(path);
                }
                snapshot.insert(file.rel.clone(), bytes);
            }
            Err(ReadFailure::Missing) => {
                changed.push(path.clone());
                if file.role == ArtifactFileRole::Companion {
                    problems.push(format!("{}: was deleted", path.display()));
                }
            }
            Err(ReadFailure::TooLarge) => {
                problems.push(format!(
                    "{}: exceeds {MAX_ARTIFACT_FILE_BYTES} bytes",
                    path.display()
                ));
                changed.push(path);
                unreadable = true;
            }
            Err(ReadFailure::Unsafe(reason)) => {
                problems.push(format!("{}: {reason}", path.display()));
                changed.push(path);
                unreadable = true;
            }
        }
    }
    if target.directory_bundle {
        for entry in walk_bundle(&target.root)? {
            if let WalkEntry::File { rel, .. } = entry {
                if !baseline.contains(rel.as_str()) {
                    changed.push(join_rel(&target.root, &rel));
                    unreadable |= read_member(&target.root, &rel, &mut problems, &mut snapshot)
                        == Member::Unsafe;
                }
            }
        }
    }
    if target.kind == ArtifactKind::Agent {
        let primary = target.primary();
        if let Some(reference) = snapshot
            .get(&primary.rel)
            .and_then(|bytes| probe_dockerfile_ref(bytes))
        {
            if !baseline.contains(reference.as_str()) {
                match companion_state(&target.root, &reference) {
                    Companion::Present => {
                        let member =
                            read_member(&target.root, &reference, &mut problems, &mut snapshot);
                        unreadable |= member == Member::Unsafe;
                        if member != Member::Missing {
                            changed.push(join_rel(&target.root, &reference));
                        }
                    }
                    Companion::Unsafe(reason) => {
                        problems.push(reason);
                        unreadable = true;
                    }
                    Companion::Missing => {}
                }
            }
        }
    }
    if !unreadable {
        problems.extend(validate_private_copy(target, &snapshot));
    }
    problems.dedup();
    Ok(InPlaceReport { changed, problems })
}

#[derive(Debug, PartialEq, Eq)]
enum Member {
    Read,
    Missing,
    Unsafe,
}

/// Bounded no-follow read of one extra bundle member into `snapshot`;
/// an unsafe or oversize member is reported in `problems`.
fn read_member(
    root: &Path,
    rel: &str,
    problems: &mut Vec<String>,
    snapshot: &mut BTreeMap<String, Vec<u8>>,
) -> Member {
    let path = join_rel(root, rel);
    match read_bounded(root, rel, MAX_ARTIFACT_FILE_BYTES) {
        Ok((bytes, _)) => {
            snapshot.insert(rel.to_string(), bytes);
            Member::Read
        }
        Err(ReadFailure::Missing) => Member::Missing,
        Err(ReadFailure::TooLarge) => {
            problems.push(format!(
                "{}: exceeds {MAX_ARTIFACT_FILE_BYTES} bytes",
                path.display()
            ));
            Member::Unsafe
        }
        Err(ReadFailure::Unsafe(reason)) => {
            problems.push(format!("{}: {reason}", path.display()));
            Member::Unsafe
        }
    }
}

/// Validate the bundle as the bounded no-follow reader sees it now, from a
/// private copy. A member that cannot be read safely is reported and stops
/// validation: the parsers must never open the live tree themselves.
fn validate_live(target: &ArtifactTarget) -> Vec<String> {
    let mut problems = Vec::new();
    let mut snapshot = BTreeMap::new();
    let mut unreadable = false;
    for file in &target.files {
        unreadable |=
            read_member(&target.root, &file.rel, &mut problems, &mut snapshot) == Member::Unsafe;
    }
    if !unreadable {
        problems.extend(validate_private_copy(target, &snapshot));
    }
    problems
}

fn validate_private_copy(
    target: &ArtifactTarget,
    snapshot: &BTreeMap<String, Vec<u8>>,
) -> Vec<String> {
    let bundle: BTreeMap<&str, &[u8]> = snapshot
        .iter()
        .map(|(rel, bytes)| (rel.as_str(), bytes.as_slice()))
        .collect();
    match crate::data::fs::edit_stage::validate_snapshot(target, &bundle) {
        Ok(found) => found,
        Err(reason) => vec![reason],
    }
}

#[cfg(test)]
#[path = "edit_target_tests.rs"]
pub(crate) mod tests;
