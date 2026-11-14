//! Layer 2 work item resolution: one entry point for every command that reads
//! a work item (`--work-item`, `specs amend <ref>`, `new spec --work-item`).
//!
//! The reference grammar is parsed in Layer 0 ([`WorkItemRef::parse`]); this
//! module decides what a *bare* reference means (it follows
//! `workItems.source`, and never tries a second source), reads local files
//! through the Layer 0 confinement helpers, and fetches remote items through
//! the Layer 1 [`IssueSourceRouter`] on a blocking task.
//!
//! Every resolved item carries a stable `key` for state and resume: `NNNN`
//! for numbered local files, `local-<hash>` for unnumbered local files and
//! `remote-<hash>` for remote and inline items, so GitHub #7 and file `0007`
//! never share state.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::data::config::work_items::{WorkItemConfigError, WorkItemSource};
use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;
use crate::data::fs::work_item_draft::MAX_WORK_ITEM_BYTES;
use crate::data::fs::work_item_files::{
    matching_work_item_files, read_local_work_item, LocalWorkItemFileError,
};
use crate::data::issue::{slugify, Issue, IssueSourceError};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::data::work_item::StagedInlineWorkItem;
use crate::data::work_item_ref::{GithubRepository, WorkItemRef, WorkItemRefError};
use crate::engine::git::GitEngine;
use crate::engine::issue::{IssueSource, IssueSourceRouter};

/// Provider key recorded for inline (request-supplied) work items.
pub const INLINE_PROVIDER: &str = "inline";

/// Hex digits of SHA-256 kept in a `remote-`/`local-` key.
const KEY_HASH_LEN: usize = 24;
/// Longest title shown to the user or used for naming.
const MAX_TITLE_CHARS: usize = 200;
/// Longest slug derived from provider data.
const MAX_SLUG_LEN: usize = 100;

/// Where a resolved work item came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemOrigin {
    /// A file in the checkout. `path` is canonical (symlinks resolved).
    Local { path: PathBuf },
    /// A tracker issue or an inline item. `provider` is `github`,
    /// `http:<name>` or `inline`; `source_id` is the canonical identity.
    Remote { source_id: String, provider: String },
}

/// A work item read from any source, ready for a command to use.
#[derive(Clone)]
pub struct ResolvedWorkItem {
    pub origin: WorkItemOrigin,
    pub number: Option<u32>,
    /// Stable identity for state and resume (see the module docs).
    pub key: String,
    /// Sanitized single-line title.
    pub title: String,
    /// Filename/ref-safe slug, already passed through `slugify`.
    pub slug: Option<String>,
    /// Rendered Markdown handed to prompts.
    pub content: String,
    /// The provider's issue (raw body and id) for remote items; `None` for
    /// local and inline items.
    pub issue: Option<Issue>,
}

impl std::fmt::Debug for ResolvedWorkItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedWorkItem")
            .field("origin", &self.origin)
            .field("number", &self.number)
            .field("key", &self.key)
            .field("title", &self.title)
            .field("slug", &self.slug)
            .field(
                "content",
                &format_args!("<{} bytes redacted>", self.content.len()),
            )
            .finish_non_exhaustive()
    }
}

impl ResolvedWorkItem {
    /// `true` for a file in the checkout.
    pub fn is_local(&self) -> bool {
        matches!(self.origin, WorkItemOrigin::Local { .. })
    }

    /// `true` for request-supplied inline content.
    pub fn is_inline(&self) -> bool {
        matches!(&self.origin, WorkItemOrigin::Remote { provider, .. } if provider == INLINE_PROVIDER)
    }

    /// The canonical identity persisted beside `key`: the remote source id,
    /// or `local:<repo-relative path>` for local files.
    pub fn source_id(&self, git_root: &Path) -> String {
        match &self.origin {
            WorkItemOrigin::Remote { source_id, .. } => source_id.clone(),
            WorkItemOrigin::Local { path } => {
                format!("local:{}", repo_relative_display(git_root, path))
            }
        }
    }

    /// Where the item came from, for display: a redacted URL (no query,
    /// fragment or credentials), `inline`, or the local path.
    pub fn source_label(&self) -> String {
        match &self.origin {
            WorkItemOrigin::Local { path } => path.display().to_string(),
            WorkItemOrigin::Remote { provider, .. } if provider == INLINE_PROVIDER => {
                "inline work item".into()
            }
            WorkItemOrigin::Remote { source_id, .. } => redact_url(source_id),
        }
    }

    /// The pre-launch notice: source and title. Work item text is untrusted
    /// input to the agent, so commands show where it came from before launch.
    pub fn launch_notice(&self) -> String {
        format!("Work item: {} — {}", self.source_label(), self.title)
    }
}

/// Where `new spec` creates a work item for the configured source.
pub enum WorkItemCreationTarget {
    /// Write `NNNN-<slug>.md` into `directory` (not created yet).
    Local { directory: PathBuf },
    /// Create an issue with `source`. `scope` is the duplicate-check scope:
    /// `github:<owner>/<repo>` (lowercase) or the provider's base URL.
    Remote {
        provider: String,
        scope: String,
        source: Arc<dyn IssueSource>,
    },
}

impl std::fmt::Debug for WorkItemCreationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local { directory } => f
                .debug_struct("Local")
                .field("directory", directory)
                .finish(),
            Self::Remote {
                provider, scope, ..
            } => f
                .debug_struct("Remote")
                .field("provider", provider)
                .field("scope", scope)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Debug)]
pub enum WorkItemError {
    /// A command that needs a work item got none.
    MissingInput,
    Reference(WorkItemRefError),
    Config(WorkItemConfigError),
    Provider(IssueSourceError),
    Files(LocalWorkItemFileError),
    Data(DataError),
    /// A bare value under the local source with no digits in it.
    InvalidLocalNumber {
        input: String,
    },
    LocalNotFound {
        number: u32,
    },
    AmbiguousWorkItem {
        number: u32,
        paths: Vec<PathBuf>,
    },
    AlreadyExists {
        reference: String,
        scope: String,
    },
    InlineReferenceConflict,
    InlineNotEditable,
    ReadOnlyContext,
    /// `workItems.source` selects GitHub but the repository has no GitHub
    /// `origin` remote. (Explicit `github:N` references report the
    /// provider's own `NoRemoteDetected` instead.)
    NoRemoteDetected {
        source: String,
    },
}

impl std::fmt::Display for WorkItemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingInput => write!(f, "a work item is required; pass --work-item <ref>"),
            Self::Reference(e) => write!(f, "invalid work item reference: {e}"),
            Self::Config(e) => write!(f, "work item configuration: {e}"),
            Self::Provider(e) => write!(f, "{e}"),
            Self::Files(e) => write!(f, "{e}"),
            Self::Data(e) => write!(f, "{e}"),
            Self::InvalidLocalNumber { input } => write!(
                f,
                "cannot read a work item number from {input:?}; \
                 use a number such as 42, or an explicit reference such as local:42, \
                 github:42 or a path"
            ),
            Self::LocalNotFound { number } => write!(
                f,
                "work item {number:04} not found in the work items directory \
                 (workItems.dir); for GitHub issue #{number} use --work-item github:{number}"
            ),
            Self::AmbiguousWorkItem { number, paths } => {
                write!(
                    f,
                    "work item {number:04} is ambiguous; more than one file matches:"
                )?;
                for path in paths {
                    write!(f, "\n  {}", path.display())?;
                }
                write!(f, "\npass the file path to choose one")
            }
            Self::AlreadyExists { reference, scope } => write!(
                f,
                "work item {reference} already exists in the configured source ({scope}); \
                 use `awman specs amend {reference}` to edit it"
            ),
            Self::InlineReferenceConflict => write!(
                f,
                "an inline work_item cannot be combined with a --work-item reference"
            ),
            Self::InlineNotEditable => write!(
                f,
                "an inline work item is read-only input and cannot be created or amended"
            ),
            Self::ReadOnlyContext => write!(
                f,
                "editing a work item needs the context(repo) overlay with write access, \
                 but configuration narrows it to read-only"
            ),
            Self::NoRemoteDetected { source } => write!(
                f,
                "workItems.source is \"{source}\", but this repository has no GitHub `origin` \
                 remote; add one, change workItems.source, or use an explicit owner/repo#N \
                 reference"
            ),
        }
    }
}

impl std::error::Error for WorkItemError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reference(e) => Some(e),
            Self::Config(e) => Some(e),
            Self::Provider(e) => Some(e),
            Self::Files(e) => Some(e),
            Self::Data(e) => Some(e),
            _ => None,
        }
    }
}

impl From<WorkItemRefError> for WorkItemError {
    fn from(e: WorkItemRefError) -> Self {
        Self::Reference(e)
    }
}
impl From<WorkItemConfigError> for WorkItemError {
    fn from(e: WorkItemConfigError) -> Self {
        Self::Config(e)
    }
}
impl From<IssueSourceError> for WorkItemError {
    fn from(e: IssueSourceError) -> Self {
        Self::Provider(e)
    }
}
impl From<LocalWorkItemFileError> for WorkItemError {
    fn from(e: LocalWorkItemFileError) -> Self {
        Self::Files(e)
    }
}
impl From<DataError> for WorkItemError {
    fn from(e: DataError) -> Self {
        Self::Data(e)
    }
}

impl WorkItemError {
    /// Process exit class: 4 for a missing resource, 2 for invalid input,
    /// configuration or ambiguity, 1 for transport and IO failures.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::LocalNotFound { .. }
            | Self::Files(LocalWorkItemFileError::NotFound { .. })
            | Self::Provider(IssueSourceError::NotFound { .. }) => 4,
            Self::MissingInput
            | Self::Reference(_)
            | Self::Config(_)
            | Self::InvalidLocalNumber { .. }
            | Self::AmbiguousWorkItem { .. }
            | Self::AlreadyExists { .. }
            | Self::InlineReferenceConflict
            | Self::InlineNotEditable
            | Self::ReadOnlyContext
            | Self::NoRemoteDetected { .. }
            | Self::Files(
                LocalWorkItemFileError::OutsideCheckout { .. }
                | LocalWorkItemFileError::NotRegular { .. }
                | LocalWorkItemFileError::TooLarge { .. }
                | LocalWorkItemFileError::InvalidUtf8 { .. },
            )
            | Self::Provider(
                IssueSourceError::InvalidRef { .. }
                | IssueSourceError::NoMatchingProvider { .. }
                | IssueSourceError::AmbiguousProvider { .. }
                | IssueSourceError::UnsafeUrl { .. }
                | IssueSourceError::NoRemoteDetected { .. },
            ) => 2,
            Self::Files(LocalWorkItemFileError::Io { .. }) | Self::Provider(_) | Self::Data(_) => 1,
        }
    }

    /// Stable snake_case failure code for API results.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingInput => "work_item_required",
            Self::Reference(_) => "invalid_work_item_reference",
            Self::Config(_) => "invalid_work_item_source",
            Self::InvalidLocalNumber { .. } => "invalid_work_item_reference",
            Self::LocalNotFound { .. } | Self::Files(LocalWorkItemFileError::NotFound { .. }) => {
                "work_item_not_found"
            }
            Self::AmbiguousWorkItem { .. } => "ambiguous_work_item",
            Self::AlreadyExists { .. } => "work_item_already_exists",
            Self::InlineReferenceConflict => "inline_work_item_conflict",
            Self::InlineNotEditable => "inline_work_item_not_editable",
            Self::ReadOnlyContext => "work_item_context_read_only",
            Self::NoRemoteDetected { .. } => "work_item_no_remote",
            Self::Files(LocalWorkItemFileError::OutsideCheckout { .. }) => {
                "work_item_outside_checkout"
            }
            Self::Files(_) => "work_item_unreadable",
            Self::Provider(e) => match e {
                IssueSourceError::NotFound { .. } => "work_item_not_found",
                IssueSourceError::Unauthorized { .. } => "work_item_unauthorized",
                IssueSourceError::Forbidden { .. } => "work_item_forbidden",
                IssueSourceError::RateLimited { .. } => "work_item_rate_limited",
                IssueSourceError::InvalidRef { .. } => "invalid_work_item_reference",
                IssueSourceError::NoRemoteDetected { .. } => "work_item_no_remote",
                IssueSourceError::NoMatchingProvider { .. } => "work_item_no_matching_provider",
                IssueSourceError::AmbiguousProvider { .. } => "work_item_ambiguous_provider",
                IssueSourceError::UnsafeUrl { .. } => "work_item_unsafe_url",
                IssueSourceError::Timeout { .. } => "work_item_timeout",
                IssueSourceError::CreateOutcomeUnknown { .. } => "work_item_create_outcome_unknown",
                IssueSourceError::ResponseTooLarge { .. } => "work_item_too_large",
                IssueSourceError::Network { .. } | IssueSourceError::ProviderError { .. } => {
                    "work_item_provider_failed"
                }
            },
            Self::Data(_) => "internal",
        }
    }
}

/// Resolves work item references for one invoking session.
///
/// Build it from the session that *invoked* the command, before any worktree
/// re-rooting, so source selection and `workItems.dir` come from that
/// session's configuration.
pub struct WorkItemResolver {
    session: Session,
    git_engine: Arc<GitEngine>,
    router: Option<Arc<IssueSourceRouter>>,
    inline: Option<StagedInlineWorkItem>,
}

impl WorkItemResolver {
    pub fn new(session: Session, git_engine: Arc<GitEngine>) -> Self {
        Self {
            session,
            git_engine,
            router: None,
            inline: None,
        }
    }

    /// Use `router` for every provider call (tests inject spy transports
    /// through [`IssueSourceRouter::with_sources`]).
    pub fn with_router(
        session: Session,
        git_engine: Arc<GitEngine>,
        router: Arc<IssueSourceRouter>,
    ) -> Self {
        Self {
            session,
            git_engine,
            router: Some(router),
            inline: None,
        }
    }

    /// Attach typed inline input (API `work_item`, pulled lease body). It
    /// resolves without a reference, independent of `workItems.source`.
    pub fn with_inline(mut self, inline: StagedInlineWorkItem) -> Self {
        self.inline = Some(inline);
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Resolve `reference` (the raw `--work-item` text). `None` with no
    /// inline input is `Ok(None)` and does no config, file or network work.
    ///
    /// Emits one notice with the source and title on success. Provider I/O
    /// runs on a blocking task; its progress messages are forwarded to
    /// `sink` from this task.
    pub async fn resolve(
        &self,
        reference: Option<&str>,
        sink: &mut dyn UserMessageSink,
    ) -> Result<Option<ResolvedWorkItem>, WorkItemError> {
        let resolved = match (self.staged_inline(), reference) {
            (Some(Staged::Typed(_)), Some(_)) => {
                return Err(WorkItemError::InlineReferenceConflict)
            }
            // Pulled leases pass no reference. Argument lists persisted before
            // the `--work-item 1` placeholder was dropped still carry one; the
            // lease body wins over it.
            // Pulled leases have no caller title: the body is the work item,
            // delivered once, exactly as the lease sent it.
            (Some(Staged::Legacy(inline)), _) => resolve_inline(&inline, false)?,
            (Some(Staged::Typed(inline)), None) => resolve_inline(&inline, true)?,
            (None, None) => return Ok(None),
            (None, Some(reference)) => self.resolve_reference(reference, sink).await?,
        };
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: resolved.launch_notice(),
        });
        Ok(Some(resolved))
    }

    async fn resolve_reference(
        &self,
        input: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ResolvedWorkItem, WorkItemError> {
        let config = self.session.effective_config();
        // Validate the configured source for every reference, explicit or
        // not: an invalid source is a hard error, never a fallback.
        let source = config.work_item_source()?;
        let parsed = WorkItemRef::parse(input)?;
        match parsed {
            WorkItemRef::Bare(value) => match &source {
                WorkItemSource::Local => {
                    let number = parse_work_item_number(&value)
                        .ok_or(WorkItemError::InvalidLocalNumber { input: value })?;
                    self.resolve_local_number(number)
                }
                WorkItemSource::Github => {
                    let number = parse_github_number(&value)?;
                    self.resolve_remote(RemoteRequest::ConfiguredGithub { number }, sink)
                        .await
                }
                WorkItemSource::Http { name } => {
                    if value.trim().is_empty() {
                        return Err(WorkItemRefError::MissingHttpId.into());
                    }
                    let reference = WorkItemRef::Http {
                        provider: name.clone(),
                        id: value,
                    };
                    self.resolve_remote(RemoteRequest::Explicit(reference), sink)
                        .await
                }
                WorkItemSource::Invalid(value) => Err(WorkItemConfigError::UnknownSource {
                    value: value.clone(),
                }
                .into()),
            },
            WorkItemRef::LocalNumber(number) => self.resolve_local_number(number),
            WorkItemRef::LocalPath(path) => self.resolve_local_path(&path),
            reference @ (WorkItemRef::Github { .. }
            | WorkItemRef::HttpUrl(_)
            | WorkItemRef::Http { .. }) => {
                self.resolve_remote(RemoteRequest::Explicit(reference), sink)
                    .await
            }
        }
    }

    fn resolve_local_number(&self, number: u32) -> Result<ResolvedWorkItem, WorkItemError> {
        let git_root = self.session.git_root();
        let path = find_work_item_file(&self.session, git_root, number)?;
        let file = read_local_work_item(git_root, self.session.working_dir(), &path)?;
        Ok(local_item(git_root, Some(number), file.path, file.content))
    }

    fn resolve_local_path(&self, path: &Path) -> Result<ResolvedWorkItem, WorkItemError> {
        let git_root = self.session.git_root();
        let file = read_local_work_item(git_root, self.session.working_dir(), path)?;
        let number = file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(leading_file_number);
        Ok(local_item(git_root, number, file.path, file.content))
    }

    async fn resolve_remote(
        &self,
        request: RemoteRequest,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ResolvedWorkItem, WorkItemError> {
        let router = self.router()?;
        let git_root = self.session.git_root().to_path_buf();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UserMessage>();
        let task = tokio::task::spawn_blocking(move || {
            let mut progress = ChannelSink(tx);
            let reference = match request {
                RemoteRequest::Explicit(reference) => reference,
                RemoteRequest::ConfiguredGithub { number } => {
                    let repository = configured_github_repository(&router, &git_root)?;
                    WorkItemRef::Github {
                        repository: Some(repository),
                        number,
                    }
                }
            };
            router
                .fetch_issue_with_progress(&reference, &git_root, &mut progress)
                .map_err(WorkItemError::from)
        });
        // The sender lives in the task, so the channel closes when it ends.
        while let Some(message) = rx.recv().await {
            sink.write_message(message);
        }
        let (issue, source) = task
            .await
            .map_err(|e| DataError::Other(format!("work item fetch task failed: {e}")))??;
        remote_item(issue, source.as_ref())
    }

    /// Where `new spec` creates a work item. Does not allocate a local
    /// number or create a directory.
    pub fn creation_target(&self) -> Result<WorkItemCreationTarget, WorkItemError> {
        let config = self.session.effective_config();
        let git_root = self.session.git_root();
        match config.work_item_source()? {
            WorkItemSource::Local => Ok(WorkItemCreationTarget::Local {
                directory: config.work_items_dir(git_root),
            }),
            WorkItemSource::Github => {
                let router = self.router()?;
                let repository = configured_github_repository(&router, git_root)?;
                Ok(WorkItemCreationTarget::Remote {
                    provider: "github".into(),
                    scope: github_scope(&repository),
                    source: router.github(),
                })
            }
            WorkItemSource::Http { name } => {
                let providers = config.work_item_providers()?;
                let provider = providers
                    .get(&name)
                    .ok_or_else(|| WorkItemConfigError::UnknownProvider { name: name.clone() })?;
                let router = self.router()?;
                Ok(WorkItemCreationTarget::Remote {
                    provider: format!("http:{name}"),
                    scope: normalized_base(&provider.base_url),
                    source: router.http(&name)?,
                })
            }
            WorkItemSource::Invalid(value) => {
                Err(WorkItemConfigError::UnknownSource { value }.into())
            }
        }
    }

    /// The provider that owns `item`, chosen from its stored provider key
    /// (never by probing).
    pub fn source_for(
        &self,
        item: &ResolvedWorkItem,
    ) -> Result<Arc<dyn IssueSource>, WorkItemError> {
        match &item.origin {
            WorkItemOrigin::Local { path } => Err(IssueSourceError::InvalidRef {
                provider: "local".into(),
                input: path.display().to_string(),
                hint: "local work items are edited in place, not through a provider".into(),
            }
            .into()),
            WorkItemOrigin::Remote { provider, .. } if provider == INLINE_PROVIDER => {
                Err(WorkItemError::InlineNotEditable)
            }
            WorkItemOrigin::Remote { provider, .. } if provider == "github" => {
                Ok(self.router()?.github())
            }
            WorkItemOrigin::Remote {
                provider,
                source_id,
            } => match provider.strip_prefix("http:") {
                Some(name) => Ok(self.router()?.http(name)?),
                None => Err(IssueSourceError::NoMatchingProvider {
                    input: source_id.clone(),
                }
                .into()),
            },
        }
    }

    /// Refuse to seed a new work item from one that already lives in the
    /// creation target (that would duplicate it); suggest `specs amend`.
    /// Seeds from another location (a local file elsewhere in the checkout,
    /// another repository, another provider base, inline) are allowed.
    pub fn ensure_new_in_target(
        &self,
        seed: &ResolvedWorkItem,
        target: &WorkItemCreationTarget,
    ) -> Result<(), WorkItemError> {
        let duplicate = match (&seed.origin, target) {
            (WorkItemOrigin::Local { path }, WorkItemCreationTarget::Local { directory }) => {
                let directory =
                    std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
                path.parent() == Some(directory.as_path())
            }
            (
                WorkItemOrigin::Remote {
                    source_id,
                    provider,
                },
                WorkItemCreationTarget::Remote { scope, .. },
            ) => {
                if provider == "github" {
                    github_issue_scope(source_id).as_deref() == Some(scope.as_str())
                } else if let Some(name) = provider.strip_prefix("http:") {
                    // Same creation scope means the same configured base: a
                    // seed from a distinct provider nested under the target's
                    // base (`items/archive` under `items`) is not a duplicate,
                    // while aliases of one base are.
                    match self.session.effective_config().work_item_providers() {
                        Ok(providers) => providers
                            .get(name)
                            .is_some_and(|p| normalized_base(&p.base_url) == *scope),
                        Err(_) => url_within_scope(source_id, scope),
                    }
                } else {
                    false
                }
            }
            _ => false,
        };
        if duplicate {
            let scope = match target {
                WorkItemCreationTarget::Local { directory } => directory.display().to_string(),
                WorkItemCreationTarget::Remote { scope, .. } => redact_url(scope),
            };
            return Err(WorkItemError::AlreadyExists {
                reference: seed_reference(seed),
                scope,
            });
        }
        Ok(())
    }

    fn router(&self) -> Result<Arc<IssueSourceRouter>, WorkItemError> {
        if let Some(router) = &self.router {
            return Ok(Arc::clone(router));
        }
        let providers = self.session.effective_config().work_item_providers()?;
        let router =
            IssueSourceRouter::new(Arc::clone(&self.git_engine), self.session.env(), &providers)?;
        Ok(Arc::new(router))
    }

    fn staged_inline(&self) -> Option<Staged> {
        if let Some(inline) = &self.inline {
            return Some(Staged::Typed(inline.clone()));
        }
        if let Some(inline) = self.session.staged_work_item() {
            return Some(Staged::Typed(inline.clone()));
        }
        // Path-only input (pulled leases): no caller title or id exists.
        self.session.inline_work_item().map(|path| {
            Staged::Legacy(StagedInlineWorkItem {
                source_id: format!(
                    "inline:path:{}",
                    sha256_hex(path.to_string_lossy().as_bytes())
                ),
                title: "Inline work item".into(),
                body_path: path.to_path_buf(),
            })
        })
    }
}

enum Staged {
    Typed(StagedInlineWorkItem),
    Legacy(StagedInlineWorkItem),
}

enum RemoteRequest {
    Explicit(WorkItemRef),
    /// A bare number under `workItems.source = github`: issue on `origin`.
    ConfiguredGithub {
        number: u32,
    },
}

/// Forwards provider progress from the blocking task to the caller's sink.
struct ChannelSink(tokio::sync::mpsc::UnboundedSender<UserMessage>);

impl UserMessageSink for ChannelSink {
    fn write_message(&mut self, msg: UserMessage) {
        let _ = self.0.send(msg);
    }
    fn replay_queued(&mut self) {}
}

fn configured_github_repository(
    router: &IssueSourceRouter,
    git_root: &Path,
) -> Result<GithubRepository, WorkItemError> {
    router.github_repository(git_root).map_err(|e| match e {
        IssueSourceError::NoRemoteDetected { .. } => WorkItemError::NoRemoteDetected {
            source: WorkItemSource::Github.as_config_value(),
        },
        other => other.into(),
    })
}

/// A bare value under the GitHub source: all ASCII digits, positive, leading
/// zeros ignored.
fn parse_github_number(value: &str) -> Result<u32, WorkItemError> {
    let invalid = || WorkItemRefError::InvalidNumber {
        input: value.to_string(),
    };
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid().into());
    }
    match value.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(invalid().into()),
    }
}

/// Extract a work item number from strings like `0069`, `69` or `WI-69`: the
/// first run of ASCII digits, as a checked `u32`.
pub fn parse_work_item_number(input: &str) -> Option<u32> {
    let digits: String = input
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// The single `NNNN-*.md` file for `number` in the session's configured work
/// items directory. No match is `LocalNotFound`; several matches are
/// `AmbiguousWorkItem` listing every path (sorted).
pub fn find_work_item_file(
    session: &Session,
    git_root: &Path,
    number: u32,
) -> Result<PathBuf, WorkItemError> {
    // The session's own merged config, not a fresh load: the command was
    // built against it.
    let directory = session.effective_config().work_items_dir(git_root);
    let mut paths = matching_work_item_files(git_root, &directory, number)?;
    match paths.len() {
        0 => Err(WorkItemError::LocalNotFound { number }),
        1 => Ok(paths.remove(0)),
        _ => Err(WorkItemError::AmbiguousWorkItem { number, paths }),
    }
}

/// State key for a numbered local file: at least four digits.
pub fn local_work_item_key(number: u32) -> String {
    format!("{number:04}")
}

/// State key for a remote (or inline) item: a short hash of its canonical
/// source id. The full id is stored beside it to reject a hash collision.
pub fn remote_work_item_key(canonical_source_id: &str) -> String {
    format!(
        "remote-{}",
        &sha256_hex(canonical_source_id.as_bytes())[..KEY_HASH_LEN]
    )
}

/// State key for an unnumbered local file: a short hash of its normalized
/// repo-relative path.
pub fn local_path_work_item_key(repo_relative_path: &str) -> String {
    format!(
        "local-{}",
        &sha256_hex(repo_relative_path.as_bytes())[..KEY_HASH_LEN]
    )
}

/// `true` when a generated key has a valid shape: digits, or `remote-`/
/// `local-` plus 24 lowercase hex digits. Check before building a path.
pub fn is_valid_work_item_key(key: &str) -> bool {
    let hashed = |rest: &str| {
        rest.len() == KEY_HASH_LEN
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if let Some(rest) = key.strip_prefix("remote-") {
        return hashed(rest);
    }
    if let Some(rest) = key.strip_prefix("local-") {
        return hashed(rest);
    }
    !key.is_empty() && key.len() <= 10 && key.bytes().all(|b| b.is_ascii_digit())
}

/// `true` when `reference` names a local file, explicitly or as a bare value
/// under the local source. Pure: no config, file or network access.
pub fn is_local_reference(reference: &WorkItemRef, source: &WorkItemSource) -> bool {
    match reference {
        WorkItemRef::LocalNumber(_) | WorkItemRef::LocalPath(_) => true,
        WorkItemRef::Bare(_) => matches!(source, WorkItemSource::Local),
        _ => false,
    }
}

/// The canonical identity of a GitHub issue URL: lowercase owner and repo.
/// Any other source id is returned unchanged.
pub fn canonical_source_id(provider: &str, source_id: &str) -> String {
    if provider == "github" {
        if let Ok(WorkItemRef::Github {
            repository: Some(repository),
            number,
        }) = WorkItemRef::parse(source_id)
        {
            return format!(
                "https://github.com/{}/{}/issues/{number}",
                repository.owner.to_ascii_lowercase(),
                repository.repo.to_ascii_lowercase()
            );
        }
    }
    source_id.to_string()
}

fn remote_item(issue: Issue, source: &dyn IssueSource) -> Result<ResolvedWorkItem, WorkItemError> {
    let source_id = canonical_source_id(&issue.provider, &issue.source_id);
    let slug = Some(slugify(&source.title_slug(&issue), MAX_SLUG_LEN)).filter(|s| !s.is_empty());
    let content = source.format_as_markdown(&issue);
    if content.len() as u64 > MAX_WORK_ITEM_BYTES {
        return Err(IssueSourceError::ResponseTooLarge {
            provider: issue.provider.clone(),
            limit: MAX_WORK_ITEM_BYTES,
        }
        .into());
    }
    Ok(ResolvedWorkItem {
        key: remote_work_item_key(&source_id),
        origin: WorkItemOrigin::Remote {
            source_id,
            provider: issue.provider.clone(),
        },
        number: issue.number,
        title: sanitize_title(&issue.title),
        slug,
        content,
        issue: Some(issue),
    })
}

fn resolve_inline(
    inline: &StagedInlineWorkItem,
    with_title: bool,
) -> Result<ResolvedWorkItem, WorkItemError> {
    let body = read_staged_body(&inline.body_path)?;
    let title = sanitize_title(&inline.title);
    let content = if !with_title {
        body
    } else if body.is_empty() {
        format!("# {title}")
    } else {
        format!("# {title}\n\n{body}")
    };
    Ok(ResolvedWorkItem {
        key: remote_work_item_key(&inline.source_id),
        origin: WorkItemOrigin::Remote {
            source_id: inline.source_id.clone(),
            provider: INLINE_PROVIDER.into(),
        },
        number: None,
        slug: Some(slugify(&title, 40)).filter(|s| !s.is_empty()),
        title,
        content,
        issue: None,
    })
}

/// Bounded strict-UTF-8 read of a privately staged inline body, refusing
/// symlinks and non-regular files.
fn read_staged_body(path: &Path) -> Result<String, WorkItemError> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|e| DataError::io(path, e))?;
    let meta = file.metadata().map_err(|e| DataError::io(path, e))?;
    if !meta.is_file() {
        return Err(LocalWorkItemFileError::NotRegular {
            path: path.to_path_buf(),
        }
        .into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_WORK_ITEM_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| DataError::io(path, e))?;
    if bytes.len() as u64 > MAX_WORK_ITEM_BYTES {
        return Err(LocalWorkItemFileError::TooLarge {
            limit: MAX_WORK_ITEM_BYTES,
        }
        .into());
    }
    String::from_utf8(bytes).map_err(|_| {
        LocalWorkItemFileError::InvalidUtf8 {
            path: path.to_path_buf(),
        }
        .into()
    })
}

fn local_item(
    git_root: &Path,
    number: Option<u32>,
    path: PathBuf,
    content: String,
) -> ResolvedWorkItem {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let title = content
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("# "))
        .map(sanitize_title)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| sanitize_title(&stem));
    let slug_source = match number {
        Some(_) => stem.split_once('-').map(|(_, rest)| rest).unwrap_or(&stem),
        None => stem.as_str(),
    };
    let key = match number {
        Some(n) => local_work_item_key(n),
        None => local_path_work_item_key(&repo_relative_display(git_root, &path)),
    };
    ResolvedWorkItem {
        origin: WorkItemOrigin::Local { path },
        number,
        key,
        title,
        slug: Some(slugify(slug_source, MAX_SLUG_LEN)).filter(|s| !s.is_empty()),
        content,
        issue: None,
    }
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` relative to `git_root` (both canonicalized when possible), with
/// forward slashes.
pub(crate) fn repo_relative_display(git_root: &Path, path: &Path) -> String {
    let root = std::fs::canonicalize(git_root).unwrap_or_else(|_| git_root.to_path_buf());
    match path
        .strip_prefix(&root)
        .or_else(|_| path.strip_prefix(git_root))
    {
        Ok(relative) => slash_path(relative),
        Err(_) => slash_path(path),
    }
}

/// `NNNN` from a filename of the form `<digits>-…`.
fn leading_file_number(name: &str) -> Option<u32> {
    let digits = name.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || name.as_bytes().get(digits) != Some(&b'-') {
        return None;
    }
    name[..digits].parse().ok()
}

/// One line, no control characters, bounded length.
fn sanitize_title(title: &str) -> String {
    let collapsed = title
        .chars()
        .filter(|c| !is_invisible_format(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut out: String = collapsed.chars().take(MAX_TITLE_CHARS).collect();
    if collapsed.chars().count() > MAX_TITLE_CHARS {
        out.push('…');
    }
    if out.is_empty() {
        out.push_str("Untitled work item");
    }
    out
}

/// Unicode format characters (category Cf) that reorder or hide text in a
/// terminal line: bidi embeddings, overrides and isolates, zero-width
/// characters, invisible operators and tag characters.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

fn redact_url(input: &str) -> String {
    crate::data::issue::display_url(input)
}

fn github_scope(repository: &GithubRepository) -> String {
    format!(
        "github:{}/{}",
        repository.owner.to_ascii_lowercase(),
        repository.repo.to_ascii_lowercase()
    )
}

fn github_issue_scope(source_id: &str) -> Option<String> {
    match WorkItemRef::parse(source_id) {
        Ok(WorkItemRef::Github {
            repository: Some(repository),
            ..
        }) => Some(github_scope(&repository)),
        _ => None,
    }
}

/// A provider base URL normalized for scope comparison.
fn normalized_base(base: &str) -> String {
    match reqwest::Url::parse(base) {
        Ok(url) => url.as_str().trim_end_matches('/').to_string(),
        Err(_) => base.trim_end_matches('/').to_string(),
    }
}

fn url_within_scope(source_id: &str, scope: &str) -> bool {
    match (reqwest::Url::parse(scope), reqwest::Url::parse(source_id)) {
        (Ok(base), Ok(candidate)) => {
            crate::engine::issue::http::url_matches_base(&base, &candidate)
        }
        _ => false,
    }
}

/// The reference a user would pass to `specs amend` for `seed`.
fn seed_reference(seed: &ResolvedWorkItem) -> String {
    match &seed.origin {
        WorkItemOrigin::Local { .. } => match seed.number {
            Some(n) => format!("local:{n}"),
            None => seed.source_label(),
        },
        WorkItemOrigin::Remote {
            source_id,
            provider,
        } => match (
            provider.as_str(),
            github_issue_scope(source_id),
            seed.number,
        ) {
            ("github", Some(scope), Some(n)) => {
                format!("{}#{n}", scope.trim_start_matches("github:"))
            }
            _ => redact_url(source_id),
        },
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn local_and_remote_numbers_never_share_a_key() {
        let local = local_work_item_key(7);
        let github = remote_work_item_key("https://github.com/acme/widgets/issues/7");
        assert_eq!(local, "0007");
        assert!(github.starts_with("remote-"));
        assert_ne!(local, github);
        assert!(is_valid_work_item_key(&local));
        assert!(is_valid_work_item_key(&github));
    }

    #[test]
    fn keys_are_stable_and_wide_numbers_are_not_truncated() {
        let id = "https://tracker.example/api/items/PROJ-123";
        assert_eq!(remote_work_item_key(id), remote_work_item_key(id));
        assert_eq!(remote_work_item_key(id).len(), "remote-".len() + 24);
        assert_ne!(
            remote_work_item_key(id),
            remote_work_item_key("https://tracker.example/api/items/PROJ-124")
        );
        assert_eq!(local_work_item_key(12345), "12345");
        assert_eq!(local_work_item_key(0), "0000");
    }

    #[test]
    fn github_identity_ignores_owner_and_repo_case() {
        let a = canonical_source_id("github", "https://github.com/Acme/Widgets/issues/7");
        let b = canonical_source_id("github", "https://github.com/acme/widgets/issues/7");
        assert_eq!(a, b);
        assert_eq!(remote_work_item_key(&a), remote_work_item_key(&b));
        // HTTP identities are already canonical and are left alone.
        let http = "https://T.example/items/A";
        assert_eq!(canonical_source_id("http:t", http), http);
    }

    #[test]
    fn key_grammar_rejects_path_like_values() {
        for bad in [
            "",
            "../0001",
            "remote-",
            "remote-ABCDEFABCDEFABCDEFABCDEF",
            "local-0123",
            "0001/..",
            "12345678901",
        ] {
            assert!(!is_valid_work_item_key(bad), "{bad:?}");
        }
        assert!(is_valid_work_item_key(&local_path_work_item_key(
            "docs/notes.md"
        )));
    }

    #[test]
    fn number_parsing_keeps_today_compatibility() {
        for (input, expected) in [
            ("WI-0144", Some(144)),
            ("0042", Some(42)),
            ("PROJ-123", Some(123)),
            ("$(codex)", None),
            ("4294967296", None),
            ("0000", Some(0)),
        ] {
            assert_eq!(parse_work_item_number(input), expected, "{input}");
        }
        assert_eq!(parse_github_number("0042").unwrap(), 42);
        assert!(parse_github_number("WI-42").is_err());
        assert!(parse_github_number("0").is_err());
        assert_eq!(leading_file_number("0042-title.md"), Some(42));
        assert_eq!(leading_file_number("notes-0042.md"), None);
        assert_eq!(leading_file_number("0042.md"), None);
    }

    #[test]
    fn local_reference_classification_is_pure() {
        let bare = WorkItemRef::Bare("42".into());
        assert!(is_local_reference(&bare, &WorkItemSource::Local));
        assert!(!is_local_reference(&bare, &WorkItemSource::Github));
        assert!(is_local_reference(
            &WorkItemRef::LocalNumber(42),
            &WorkItemSource::Github
        ));
        assert!(!is_local_reference(
            &WorkItemRef::Github {
                repository: None,
                number: 42
            },
            &WorkItemSource::Local
        ));
    }

    #[test]
    fn titles_are_single_line_and_bounded() {
        assert_eq!(sanitize_title("a\n\tb\u{1b}[31m  c"), "a b [31m c");
        assert_eq!(sanitize_title("   "), "Untitled work item");
        assert_eq!(sanitize_title(&"x".repeat(500)).chars().count(), 201);
        // Bidi overrides, isolates and zero-width characters cannot reorder
        // or hide text in the launch notice.
        assert_eq!(
            sanitize_title("fix\u{202E}gnp.exe\u{200B}\u{2066}x\u{2069}\u{FEFF}"),
            "fixgnp.exex"
        );
    }
}
