//! `WorkflowPrerequisiteFlow` — turns a prerequisite report, the resolved
//! policy and a frontend into the run's `PrerequisiteGrants`.
//!
//! The engine (`PrerequisitePreflight`) only probes and reports. Every
//! decision is made here, in one order for every credential:
//!
//! 1. an explicit `--prerequisite-*` flag (a flag value *is* a grant, so the
//!    question is skipped, on a TTY too; `--yes` never grants);
//! 2. a `deny` policy, which is never asked about;
//! 3. a remembered decision from `~/.awman/prerequisite-grants.json`;
//! 4. an `allow` policy (only global config, a squad task or a flag can set
//!    one for a secret; repo config only narrows);
//! 5. a question, when a person is at the frontend;
//! 6. otherwise the headless answer, which is always "deny".
//!
//! Grants live in [`PreparedPrerequisites`], held by `ExecWorkflowCommand`
//! for the run and passed down. They are never put on a `Session`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::command::commands::agent_trust::TrustDecision;
use crate::command::dispatch::catalogue::FrontendKind;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::data::config::flags::{IdentityFlag, PrerequisiteFlags, SshFlag, TokenFlag};
use crate::data::config::global::GlobalConfig;
use crate::data::config::workflows::{
    AcquirePolicy, CredentialUse, PolicySource, PrerequisitePolicy, UnresolvedMode,
};
use crate::data::fs::agent_trust::{TrustKey, TrustStore};
use crate::data::fs::prerequisite_grants::{
    GrantKey, PrerequisiteGrantStore, RememberedGrant, RememberedKind,
};
use crate::data::fs::ssh_keys::SshKeyInventory;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::{Session, SessionType};
use crate::data::workflow_definition::Workflow;
use crate::data::workflow_prerequisites::{
    AcquireScope, GitRemoteUrl, PrerequisiteCheck, PrerequisiteOrigin, PrerequisitePhase,
    PrerequisiteStatus, Remediation, WorkflowPrerequisite, WorkflowPrerequisiteReport,
    WorkflowPrerequisites,
};
use crate::engine::auth::credential::SecretString;
use crate::engine::auth::github_token::{
    GitHubTokenResolver, GitHubTokenResolverOptions, GITHUB_TOKEN_PROBE_TIMEOUT,
};
use crate::engine::auth::host_secret_command::{HostSecretCommand, HostStdin};
use crate::engine::auth::redaction::RedactionSet;
use crate::engine::workflow::prerequisite_grants::{
    DroppedGrant, GitIdentityGrant, IdentitySource, PrerequisiteGrants, SshGrant, TokenGrant,
    TokenGrantSource,
};
use crate::engine::workflow::prerequisites::{
    PreflightSources, PrerequisitePreflight, PrerequisitePreflightOptions, RememberedGrants,
};

use super::frontend::{
    GapChoice, IdentityChoice, IdentityPersist, SshChoice, TokenConsent, TokenSourceChoice,
    ToolChoice, WorkflowPrerequisiteFrontend,
};

/// The trust-store name a repository's `acquire` command is approved under,
/// keyed by repo root + the SHA-256 of its JSON argv.
pub const ACQUIRE_TRUST_NAME: &str = "workflows.prerequisites.githubToken.acquire";

/// How many times the token-source question is asked after a source fails.
const TOKEN_SOURCE_ATTEMPTS: usize = 3;

/// Everything the flow needs that is fixed for one run.
#[derive(Debug, Clone)]
pub struct WorkflowPrerequisiteFlowOptions {
    pub frontend_kind: FrontendKind,
    /// The headless profile of this host (api/squad/cli).
    pub headless: HeadlessDefaults,
    pub origin: PrerequisiteOrigin,
    /// The resolved policy, flags already merged (`EffectiveConfig::prerequisite_policy`).
    pub policy: PrerequisitePolicy,
    pub flags: PrerequisiteFlags,
    pub git_root: PathBuf,
    pub worktree: Option<PathBuf>,
    pub worktree_planned: bool,
    pub phase_image: Option<String>,
    /// Every custom agent definition's binary (and name): refused by
    /// `HostSecretCommand` so a token command can never launch an agent.
    pub custom_agent_binaries: Vec<String>,
    /// "repo config mounts ~/.ssh into every step" and similar.
    pub repo_overlay_notes: Vec<String>,
}

impl WorkflowPrerequisiteFlowOptions {
    /// Options for `session` with `flags` (and a squad task's stored policy)
    /// layered by the one Layer 0 merge.
    pub fn for_session(
        session: &Session,
        frontend_kind: FrontendKind,
        flags: &PrerequisiteFlags,
        squad_task: Option<&PrerequisitePolicy>,
        origin: PrerequisiteOrigin,
        phase_image: Option<String>,
    ) -> Self {
        Self {
            frontend_kind,
            headless: headless_for(frontend_kind),
            origin: if frontend_kind == FrontendKind::SquadDaemon {
                PrerequisiteOrigin::AssumedAll
            } else {
                origin
            },
            policy: {
                let mut session = session.clone();
                let mut config = session.flags().clone();
                config.squad_prerequisites = frontend_kind == FrontendKind::SquadDaemon;
                session.set_flags(config);
                policy_for(&session, flags, squad_task)
            },
            flags: flags.clone(),
            git_root: session.git_root().to_path_buf(),
            worktree: None,
            worktree_planned: false,
            phase_image,
            custom_agent_binaries: custom_agent_binaries(session),
            repo_overlay_notes: repo_overlay_notes(session),
        }
    }
}

/// The headless profile a frontend kind answers from.
pub fn headless_for(kind: FrontendKind) -> HeadlessDefaults {
    match kind {
        FrontendKind::Api => HeadlessDefaults::api(),
        FrontendKind::SquadDaemon => HeadlessDefaults::squad(
            crate::command::commands::mount_scope::MountScopeDecision::MountGitRoot,
        ),
        FrontendKind::Cli | FrontendKind::Tui => HeadlessDefaults::cli(),
    }
}

/// `session`'s prerequisite policy with `flags` and `squad_task` as the top
/// layers. The merge itself is Layer 0's; this only supplies the layers.
pub fn policy_for(
    session: &Session,
    flags: &PrerequisiteFlags,
    squad_task: Option<&PrerequisitePolicy>,
) -> PrerequisitePolicy {
    let mut session = session.clone();
    let mut config = session.flags().clone();
    config.prerequisites = flags.clone();
    config.squad_task_prerequisites = squad_task.cloned();
    session.set_flags(config);
    session.prerequisite_policy()
}

/// Every custom agent definition's binary and name, for the host-command
/// refusal list. First-party agents are refused by `HostSecretCommand` itself.
pub fn custom_agent_binaries(session: &Session) -> Vec<String> {
    use crate::engine::agent::registry::AgentRegistry;
    let env = session.env();
    let root = session.definition_root();
    let listing = AgentRegistry::list(env, Some(root));
    let mut out = Vec::new();
    for custom in &listing.custom {
        out.push(custom.name.clone());
        let Ok(name) = crate::data::session::AgentName::new(&custom.name) else {
            continue;
        };
        if let Ok(untrusted) = AgentRegistry::load_untrusted(env, &name, Some(root)) {
            let def = untrusted.definition();
            let mut binaries: Vec<String> = def
                .interactive_entrypoint
                .first()
                .cloned()
                .into_iter()
                .collect();
            if let crate::engine::agent::definition::AgentImageSource::Rendered { spec } =
                &def.image
            {
                binaries.push(spec.binary.clone());
            }
            for binary in binaries {
                if !binary.is_empty() && !out.contains(&binary) {
                    out.push(binary);
                }
            }
        }
    }
    out
}

/// Credential-bearing overlays the repository's `.awman/config.json` adds to
/// every step, so the summary shows what the checkout itself mounts.
pub fn repo_overlay_notes(session: &Session) -> Vec<String> {
    let Some(overlays) = session.repo_config().overlays.as_ref() else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for overlay in overlays {
        let trimmed = overlay.trim();
        if trimmed.starts_with("ssh(") {
            notes.push("repo config mounts ~/.ssh into every step (overlay ssh())".to_string());
        } else if let Some(rest) = trimmed.strip_prefix("env(") {
            let name = rest.trim_end_matches(')').trim();
            if looks_like_secret_name(name) {
                notes.push(format!(
                    "repo config passes the host variable {name} into every step (overlay env({name}))"
                ));
            }
        }
    }
    notes
}

fn looks_like_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "KEY", "PASSWORD", "PASS", "CREDENTIAL"]
        .iter()
        .any(|marker| upper.contains(marker))
}

/// What one prerequisite came to after the flow.
pub struct PrerequisiteOutcome {
    /// The report with every granted check marked satisfied.
    pub report: WorkflowPrerequisiteReport,
    pub grants: PrerequisiteGrants,
    /// Still missing or awaiting consent after prompts and policy.
    pub unresolved: Vec<WorkflowPrerequisite>,
}

/// The prerequisites of one run, carried by `PreparedRun`. Grants live here
/// for the run and nowhere else.
pub struct PreparedPrerequisites {
    /// The needs after classification (`PushAuth` resolved to SSH/HTTPS).
    pub needs: WorkflowPrerequisites,
    pub report: WorkflowPrerequisiteReport,
    pub grants: PrerequisiteGrants,
    pub flow: WorkflowPrerequisiteFlow,
    /// The run's single redaction set: every granted token value is in it.
    pub redaction: RedactionSet,
    /// Remote name (or URL) → credential-stripped URL, for grant targets.
    pub remotes: BTreeMap<String, GitRemoteUrl>,
}

impl PreparedPrerequisites {
    /// The classified URL of a step's remote: a remote name, or a URL given
    /// directly (`clone_repo`).
    pub fn remote(&self, remote_or_url: &str) -> Option<GitRemoteUrl> {
        self.remotes
            .get(remote_or_url)
            .cloned()
            .or_else(|| is_url_like(remote_or_url).then(|| GitRemoteUrl::parse(remote_or_url)))
    }
}

impl std::fmt::Debug for PreparedPrerequisites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPrerequisites")
            .field("needs", &self.needs)
            .field("grants", &self.grants)
            .finish_non_exhaustive()
    }
}

fn is_url_like(remote: &str) -> bool {
    remote.contains("://") || remote.contains(':') || remote.starts_with(['/', '.'])
}

/// What resolving one check did to it.
enum Resolution {
    /// Satisfied now; `Some` replaces the explanation.
    Satisfied(Option<String>),
    /// Declined or impossible; the check becomes `Missing` with this text.
    Declined(String),
    /// Leave the check as the preflight reported it.
    Unchanged,
}

pub struct WorkflowPrerequisiteFlow {
    workflow_git_source: Option<crate::engine::git::remote_push::WorkflowGitSource>,
    decision_remote: String,
    engines: Engines,
    session: Session,
    options: WorkflowPrerequisiteFlowOptions,
    grant_store: Option<PrerequisiteGrantStore>,
    data_home: Option<PathBuf>,
    home: PathBuf,
}

impl WorkflowPrerequisiteFlow {
    pub fn new(
        engines: &Engines,
        session: &Session,
        options: WorkflowPrerequisiteFlowOptions,
    ) -> Self {
        let data_home = GlobalConfig::data_home_with(session.env()).ok();
        let grant_store = data_home.as_deref().map(PrerequisiteGrantStore::load);
        let home = session
            .env()
            .home_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            workflow_git_source: engines
                .git_engine
                .capture_workflow_git_source(
                    options.worktree.as_deref().unwrap_or(&options.git_root),
                )
                .ok(),
            decision_remote: "origin".into(),
            engines: engines.clone(),
            session: session.clone(),
            options,
            grant_store,
            data_home,
            home,
        }
    }

    pub fn options(&self) -> &WorkflowPrerequisiteFlowOptions {
        &self.options
    }

    /// Point the probes at the run's worktree once it exists.
    pub fn set_worktree(&mut self, worktree: Option<PathBuf>) {
        if self.options.worktree != worktree {
            self.workflow_git_source = self
                .engines
                .git_engine
                .capture_workflow_git_source(worktree.as_deref().unwrap_or(&self.options.git_root))
                .ok();
        }
        self.options.worktree = worktree;
    }

    pub fn workflow_git_source(
        &self,
    ) -> Option<&crate::engine::git::remote_push::WorkflowGitSource> {
        self.workflow_git_source.as_ref()
    }

    /// `--check-prerequisites`: 1 when anything is missing, else 0.
    pub fn check_only_exit_code(report: &WorkflowPrerequisiteReport) -> i32 {
        if report
            .checks
            .iter()
            .any(|c| c.status != PrerequisiteStatus::Satisfied)
        {
            1
        } else {
            0
        }
    }

    fn repo(&self) -> &Path {
        self.options
            .worktree
            .as_deref()
            .unwrap_or(&self.options.git_root)
    }

    /// The credential-stripped URL of `remote`: the session's `repo_url` for
    /// a remote API session, otherwise `git remote get-url`.
    fn remote_url(&self, remote: &str) -> Option<GitRemoteUrl> {
        if is_url_like(remote) {
            return Some(GitRemoteUrl::parse(remote));
        }
        if let SessionType::Remote { repo_url, .. } = self.session.session_type() {
            return (remote == "origin").then(|| GitRemoteUrl::parse(repo_url));
        }
        self.engines
            .git_engine
            .remote_url_redacted(self.repo(), remote)
            .ok()
            .flatten()
            .map(|url| GitRemoteUrl::parse(&url))
    }

    fn grant_key(&self, kind: RememberedKind) -> GrantKey {
        GrantKey {
            git_root: std::fs::canonicalize(&self.options.git_root)
                .unwrap_or_else(|_| self.options.git_root.clone()),
            remote_url: self
                .remote_url(&self.decision_remote)
                .map(|u| u.redacted)
                .unwrap_or_default(),
            prerequisite: kind,
        }
    }

    fn remembered(&self) -> RememberedGrants {
        let mut out = RememberedGrants::default();
        let Some(store) = &self.grant_store else {
            return out;
        };
        match store.lookup(&self.grant_key(RememberedKind::Ssh)) {
            Some(RememberedGrant::SshKey { key }) => out.ssh_key = Some(key.clone()),
            Some(RememberedGrant::SshDir) => out.ssh_dir = true,
            _ => {}
        }
        if let Some(RememberedGrant::TokenEnv { var }) =
            store.lookup(&self.grant_key(RememberedKind::GithubToken))
        {
            out.token_env = Some(var.clone());
        }
        if let Some(RememberedGrant::Identity { name, email }) =
            store.lookup(&self.grant_key(RememberedKind::GitIdentity))
        {
            out.identity = Some((name.clone(), email.clone()));
        }
        out
    }

    fn remember(
        &mut self,
        kind: RememberedKind,
        grant: RememberedGrant,
        sink: &mut dyn WorkflowPrerequisiteFrontend,
    ) {
        let key = self.grant_key(kind);
        let Some(store) = self.grant_store.as_mut() else {
            sink.write_message(warning(
                "could not locate ~/.awman to remember this decision; it applies to this run only",
            ));
            return;
        };
        if let Err(e) = store.remember(key, grant) {
            sink.write_message(warning(format!(
                "could not remember this decision ({e}); it applies to this run only"
            )));
        }
    }

    fn trust_store(&self) -> Option<TrustStore> {
        self.data_home.as_deref().map(TrustStore::load)
    }

    fn acquire_trust_key(&self) -> TrustKey {
        TrustKey::Repo {
            git_root: self.options.git_root.clone(),
            name: ACQUIRE_TRUST_NAME.to_string(),
        }
    }

    /// Whether a repository-scoped `acquire` command is approved for its
    /// current content hash.
    fn repo_acquire_approved(&self) -> bool {
        let Some(acquire) = &self.options.policy.github_token.acquire else {
            return false;
        };
        if acquire.scope != AcquireScope::Repo {
            return false;
        }
        self.resolver("github.com")
            .acquire_command(
                acquire,
                HostStdin::Null,
                &self.options.custom_agent_binaries,
            )
            .ok()
            .is_some_and(|cmd| {
                self.trust_store()
                    .is_some_and(|store| store.is_trusted(&self.acquire_trust_key(), &cmd.sha256()))
            })
    }

    fn notes(&self) -> Vec<String> {
        let policy = &self.options.policy;
        let mut notes: Vec<String> = policy.warnings.clone();
        if policy.git_identity.use_ == CredentialUse::Allow
            && policy.git_identity.fixed.is_some()
            && policy.git_identity.source == PolicySource::Repo
        {
            notes.push("identity from repo config".to_string());
        }
        if let Some(acquire) = &policy.github_token.acquire {
            let rendered = shell_words::join(&acquire.argv);
            match acquire.scope {
                AcquireScope::Global => notes.push(format!(
                    "global token command (runs ON YOUR HOST when needed): {rendered}"
                )),
                AcquireScope::Repo => notes.push(format!(
                    "repo token command (needs your approval; runs ON YOUR HOST): {rendered}"
                )),
            }
        }
        notes.extend(self.options.repo_overlay_notes.iter().cloned());
        notes
    }

    fn resolver(&self, host: &str) -> GitHubTokenResolver {
        self.engines
            .auth_engine
            .github_token_resolver(GitHubTokenResolverOptions {
                host: host.to_string(),
                env_vars: self.options.policy.github_token.env_vars.clone(),
                verify: self.options.policy.github_token.verify,
                timeout: GITHUB_TOKEN_PROBE_TIMEOUT,
            })
            .with_refused_binaries(self.options.custom_agent_binaries.clone())
            .with_repository_root(self.options.git_root.clone())
    }

    /// The preflight for `needs`, with production sources.
    pub fn build_preflight(
        &self,
        needs: WorkflowPrerequisites,
        phase: PrerequisitePhase,
        can_ask: bool,
        existing_grants: Option<PrerequisiteGrants>,
    ) -> PrerequisitePreflight {
        let mut options = PrerequisitePreflightOptions::for_session(
            needs,
            &self.session,
            std::sync::Arc::clone(&self.engines.runtime),
            self.engines.container_runtime.clone(),
            self.options.policy.clone(),
        );
        options.phase = phase;
        options.worktree_planned = self.options.worktree_planned;
        options.git_root = self.options.git_root.clone();
        options.worktree = self.options.worktree.clone();
        options.phase_image = self.options.phase_image.clone();
        options.remembered = self.remembered();
        options.repo_acquire_approved = self.repo_acquire_approved();
        options.can_ask = can_ask;
        options.existing_grants = existing_grants;
        options.notes = self.notes();
        let cache = self
            .data_home
            .as_deref()
            .map(crate::data::fs::phase_tools_cache::PhaseToolsCache::load)
            .unwrap_or_else(crate::data::fs::phase_tools_cache::PhaseToolsCache::in_memory);
        let sources = PreflightSources::production(
            std::sync::Arc::clone(&self.engines.git_engine),
            self.home.clone(),
            self.resolver("github.com"),
            self.engines.container_runtime.clone(),
            cache,
        );
        PrerequisitePreflight::new(options, sources)
    }

    /// Probe `needs` and return the report. Never asks anything.
    pub fn preflight(
        &self,
        needs: &WorkflowPrerequisites,
        can_ask: bool,
    ) -> WorkflowPrerequisiteReport {
        self.build_preflight(needs.clone(), PrerequisitePhase::Start, can_ask, None)
            .run()
    }

    /// Run the whole start-of-run flow: probe, show, ask, apply policy.
    /// Returns everything the run carries, or the error that stops it before
    /// any container starts.
    pub fn prepare(
        mut self,
        needs: &WorkflowPrerequisites,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> Result<PreparedPrerequisites, CommandError> {
        let can_ask = frontend.can_ask_prerequisites();
        let preflight =
            self.build_preflight(needs.clone(), PrerequisitePhase::Start, can_ask, None);
        let report = preflight.run();
        let resolved_needs = preflight.resolved_prerequisites(&report);
        let redaction = RedactionSet::new();
        let outcome = self.resolve(&resolved_needs, report, &redaction, frontend)?;
        let remotes = self.remotes_for(&resolved_needs);
        Ok(PreparedPrerequisites {
            needs: resolved_needs,
            report: outcome.report,
            grants: outcome.grants,
            flow: self,
            redaction,
            remotes,
        })
    }

    pub fn remotes_for(&self, needs: &WorkflowPrerequisites) -> BTreeMap<String, GitRemoteUrl> {
        let mut names = vec!["origin".to_string()];
        for need in &needs.needs {
            match &need.prerequisite {
                WorkflowPrerequisite::GitRemote { remote }
                | WorkflowPrerequisite::PushAuth { remote }
                | WorkflowPrerequisite::PushAuthSsh { remote, .. }
                | WorkflowPrerequisite::PushAuthHttps { remote, .. } => {
                    if !names.contains(remote) {
                        names.push(remote.clone());
                    }
                }
                _ => {}
            }
        }
        names
            .into_iter()
            .filter_map(|name| self.remote_url(&name).map(|url| (name, url)))
            .collect()
    }

    /// Show the report, decide every check, then apply `unresolved`.
    ///
    /// `Fail` ⇒ `Err(WorkflowPrerequisitesUnresolved)`; `Warn` ⇒ one warning
    /// per unresolved prerequisite. An `Abort` answer ⇒ `Err(Aborted)`. Every
    /// granted token is added to `redaction`.
    pub fn resolve(
        &mut self,
        needs: &WorkflowPrerequisites,
        report: WorkflowPrerequisiteReport,
        redaction: &RedactionSet,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> Result<PrerequisiteOutcome, CommandError> {
        let default = if frontend.can_ask_prerequisites() {
            UnresolvedMode::Warn
        } else {
            self.options.headless.prerequisite_unresolved(
                if self.options.frontend_kind == FrontendKind::SquadDaemon {
                    PrerequisiteOrigin::AssumedAll
                } else {
                    needs.origin
                },
            )
        };
        let outcome = self.decide(
            report,
            PrerequisiteGrants::default(),
            redaction,
            frontend,
            true,
        )?;
        let mode = self.options.policy.unresolved_or(default);
        if !outcome.unresolved.is_empty() {
            if mode == UnresolvedMode::Fail {
                return Err(CommandError::WorkflowPrerequisitesUnresolved {
                    report: Box::new(outcome.report),
                });
            }
            for check in outcome.report.needs_attention() {
                frontend.write_message(warning(format!(
                    "workflow prerequisite not satisfied: {}: {}",
                    check.prerequisite.label(),
                    check.explanation
                )));
            }
        }
        Ok(outcome)
    }

    /// Decide every check of `report`, starting from `grants`.
    fn decide(
        &mut self,
        mut report: WorkflowPrerequisiteReport,
        mut grants: PrerequisiteGrants,
        redaction: &RedactionSet,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
        show: bool,
    ) -> Result<PrerequisiteOutcome, CommandError> {
        let can_ask = frontend.can_ask_prerequisites();
        if show && can_ask {
            frontend.show_workflow_prerequisites(&report)?;
        }
        // One token serves both `GitHubToken` and `PushAuthHttps`: once it is
        // declined, the second check is not asked about again.
        let mut token_declined = false;
        for index in 0..report.checks.len() {
            let check = report.checks[index].clone();
            self.decision_remote = match &check.prerequisite {
                WorkflowPrerequisite::PushAuthSsh { remote, .. }
                | WorkflowPrerequisite::PushAuthHttps { remote, .. } => remote.clone(),
                _ => "origin".into(),
            };
            let resolution = match &check.prerequisite {
                WorkflowPrerequisite::GitIdentity => {
                    self.resolve_identity(&check, &mut grants, frontend, can_ask)?
                }
                WorkflowPrerequisite::PushAuthSsh { remote, .. } => {
                    self.resolve_ssh(&check, remote, &mut grants, frontend, can_ask)?
                }
                WorkflowPrerequisite::GitHubToken { host }
                | WorkflowPrerequisite::PushAuthHttps { host, .. } => {
                    if token_declined && grants.token.is_none() {
                        Resolution::Declined("no GitHub token was granted (declined above)".into())
                    } else {
                        let resolution =
                            self.resolve_token(&check, host, &mut grants, frontend, can_ask)?;
                        token_declined |= matches!(resolution, Resolution::Declined(_));
                        resolution
                    }
                }
                WorkflowPrerequisite::ContainerTool { .. } => {
                    if check.status == PrerequisiteStatus::Missing {
                        let choice = if can_ask {
                            frontend.ask_prerequisite_tool(
                                &crate::command::prompts::prerequisite_missing_tool(&check),
                            )?
                        } else {
                            self.options.headless.prerequisite_tool()
                        };
                        if choice == ToolChoice::Abort {
                            return Err(CommandError::Aborted);
                        }
                    }
                    Resolution::Unchanged
                }
                WorkflowPrerequisite::GitRemote { .. }
                | WorkflowPrerequisite::PushAuth { .. }
                | WorkflowPrerequisite::PushedBranch
                | WorkflowPrerequisite::NotDetachedHead => {
                    if check.status == PrerequisiteStatus::Missing {
                        let choice = if can_ask {
                            frontend.ask_prerequisite_gap(
                                &crate::command::prompts::prerequisite_gap(&check),
                            )?
                        } else {
                            self.options.headless.prerequisite_gap()
                        };
                        if choice == GapChoice::Abort {
                            return Err(CommandError::Aborted);
                        }
                    }
                    Resolution::Unchanged
                }
            };
            let slot = &mut report.checks[index];
            match resolution {
                Resolution::Satisfied(explanation) => {
                    if let Some(remote) = self.remote_url(&self.decision_remote) {
                        match check.prerequisite {
                            WorkflowPrerequisite::PushAuthSsh { .. } if grants.ssh.is_some() => {
                                if let Ok(transport) = self
                                    .engines
                                    .git_engine
                                    .workflow_ssh_destination(self.repo(), &self.decision_remote)
                                {
                                    grants
                                        .ssh_transports
                                        .insert(remote.redacted.clone(), transport);
                                }
                                grants.ssh_destinations.insert(remote.redacted);
                            }
                            WorkflowPrerequisite::PushAuthHttps { .. }
                            | WorkflowPrerequisite::GitHubToken { .. }
                                if grants.token.is_some() =>
                            {
                                grants.token_destinations.insert(remote.redacted);
                            }
                            _ => {}
                        }
                    }
                    slot.status = PrerequisiteStatus::Satisfied;
                    if let Some(text) = explanation {
                        slot.explanation = text;
                    }
                    slot.remediations.clear();
                }
                Resolution::Declined(text) => {
                    slot.status = PrerequisiteStatus::Missing;
                    slot.explanation = text;
                }
                Resolution::Unchanged => {}
            }
        }
        for secret in grants.redaction_secrets() {
            redaction.add(secret);
        }
        let report = PrerequisitePreflight::redact_report(report, redaction);
        if show && !can_ask {
            frontend.show_workflow_prerequisites(&report)?;
        }
        let unresolved = report
            .needs_attention()
            .into_iter()
            .map(|c| c.prerequisite.clone())
            .collect();
        Ok(PrerequisiteOutcome {
            report,
            grants,
            unresolved,
        })
    }

    // ─── Identity ───────────────────────────────────────────────────────────

    fn resolve_identity(
        &mut self,
        check: &PrerequisiteCheck,
        grants: &mut PrerequisiteGrants,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
        can_ask: bool,
    ) -> Result<Resolution, CommandError> {
        let grant = |grants: &mut PrerequisiteGrants, name: &str, email: &str, source| {
            grants.identity = Some(GitIdentityGrant {
                name: name.to_string(),
                email: email.to_string(),
                source,
            });
            Resolution::Satisfied(Some(format!(
                "identity \"{name} <{email}>\" injected into commit_changes containers"
            )))
        };
        if grants.identity.is_some() {
            return Ok(Resolution::Satisfied(None));
        }
        match &self.options.flags.identity {
            Some(IdentityFlag::Deny) => {
                return Ok(if check.status == PrerequisiteStatus::Satisfied {
                    Resolution::Unchanged
                } else {
                    Resolution::Declined(
                        "identity injection denied by --prerequisite-identity deny; commit_changes will fail"
                            .into(),
                    )
                })
            }
            Some(IdentityFlag::Fixed { name, email }) => {
                return Ok(grant(grants, name, email, IdentitySource::Flag))
            }
            Some(IdentityFlag::Host) => {
                return Ok(match self.host_identity() {
                    Some((name, email)) => grant(grants, &name, &email, IdentitySource::Flag),
                    None => Resolution::Declined(
                        "--prerequisite-identity host: the host has no global user.name/user.email"
                            .into(),
                    ),
                })
            }
            None => {}
        }
        let policy = self.options.policy.git_identity.clone();
        if policy.use_ == CredentialUse::Deny {
            return Ok(Resolution::Unchanged);
        }
        if policy.use_ == CredentialUse::Allow {
            if let Some(fixed) = &policy.fixed {
                let source = if policy.source == PolicySource::Repo {
                    IdentitySource::RepoConfig
                } else {
                    IdentitySource::GlobalConfig
                };
                return Ok(grant(grants, &fixed.name, &fixed.email, source));
            }
        }
        if let Some((name, email)) = self.remembered().identity {
            return Ok(grant(grants, &name, &email, IdentitySource::Remembered));
        }
        if check.status == PrerequisiteStatus::Satisfied {
            // The container sees a repository-local identity already.
            return Ok(Resolution::Unchanged);
        }
        if policy.use_ == CredentialUse::Allow {
            if let Some((name, email)) = self.host_identity() {
                return Ok(grant(grants, &name, &email, IdentitySource::Host));
            }
        }
        if !can_ask || check.status == PrerequisiteStatus::Unknown {
            return Ok(if can_ask {
                Resolution::Unchanged
            } else {
                headless_declined(check, "git identity")
            });
        }
        let prompt = crate::command::prompts::prerequisite_identity(check);
        match frontend.ask_prerequisite_identity(&prompt)? {
            IdentityChoice::UseHost { name, email } => {
                Ok(grant(grants, &name, &email, IdentitySource::Host))
            }
            IdentityChoice::Enter => {
                let Some(name) = frontend.ask_prerequisite_identity_text(
                    &crate::command::prompts::prerequisite_identity_name(),
                )?
                else {
                    return Ok(declined_identity());
                };
                let Some(email) = frontend.ask_prerequisite_identity_text(
                    &crate::command::prompts::prerequisite_identity_email(),
                )?
                else {
                    return Ok(declined_identity());
                };
                let (name, email) = (name.trim().to_string(), email.trim().to_string());
                if let Err(reason) = super::flags::validate_identity(&name, &email) {
                    frontend.write_message(warning(format!(
                        "identity not used: {reason}; commit_changes will fail"
                    )));
                    return Ok(declined_identity());
                }
                Ok(grant(grants, &name, &email, IdentitySource::Entered))
            }
            IdentityChoice::SetLocalFromHost { name, email } => {
                let command = check
                    .remediations
                    .iter()
                    .find_map(|r| match r {
                        Remediation::SetLocalIdentity { command, .. } => Some(command.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        crate::engine::git::GitEngine::set_local_identity_command_display(
                            self.repo(),
                            &name,
                            &email,
                        )
                    });
                let persist = frontend.ask_prerequisite_identity_persist(
                    &crate::command::prompts::prerequisite_identity_persist(&command),
                )?;
                if persist == IdentityPersist::RunOnly {
                    return Ok(grant(grants, &name, &email, IdentitySource::Host));
                }
                match self
                    .engines
                    .git_engine
                    .set_local_identity(self.repo(), &name, &email)
                {
                    Ok(()) => Ok(Resolution::Satisfied(Some(format!(
                        "identity \"{name} <{email}>\" written to the repository's .git/config"
                    )))),
                    Err(e) => {
                        frontend.write_message(warning(format!(
                            "could not write the identity to .git/config ({e}); using it for this run only"
                        )));
                        Ok(grant(grants, &name, &email, IdentitySource::Host))
                    }
                }
            }
            IdentityChoice::Continue => {
                frontend.write_message(warning(
                    "continuing without a git identity: commit_changes will fail",
                ));
                Ok(declined_identity())
            }
        }
    }

    fn host_identity(&self) -> Option<(String, String)> {
        let identity = self.engines.git_engine.host_identity().ok()?;
        match (identity.name, identity.email) {
            (Some(name), Some(email)) if !name.trim().is_empty() && !email.trim().is_empty() => {
                Some((name, email))
            }
            _ => None,
        }
    }

    // ─── SSH ────────────────────────────────────────────────────────────────

    fn ssh_key_grant(&self, requested: &Path) -> Result<SshGrant, String> {
        let path = SshKeyInventory::validate_key_path(&self.home, requested)
            .map_err(|e| format!("SSH key {} cannot be used: {e}", requested.display()))?;
        let inventory = SshKeyInventory::scan(&self.home);
        let passphrase_protected = inventory.find(&path).and_then(|k| k.passphrase_protected);
        Ok(SshGrant::Key {
            key_path: path,
            known_hosts: inventory.known_hosts.clone(),
            passphrase_protected,
        })
    }

    fn whole_ssh_dir(&self) -> SshGrant {
        SshGrant::WholeDir {
            ssh_dir: self.home.join(".ssh"),
        }
    }

    fn resolve_ssh(
        &mut self,
        check: &PrerequisiteCheck,
        remote: &str,
        grants: &mut PrerequisiteGrants,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
        can_ask: bool,
    ) -> Result<Resolution, CommandError> {
        if grants.ssh.is_some()
            && self
                .remote_url(remote)
                .is_some_and(|u| grants.allows_ssh(&u.redacted))
        {
            return Ok(Resolution::Satisfied(None));
        }
        let granted = |grants: &mut PrerequisiteGrants, grant: SshGrant| {
            let text = match &grant {
                SshGrant::Key { key_path, .. } => format!(
                    "SSH key {} mounted read-only into push containers",
                    key_path.display()
                ),
                SshGrant::WholeDir { .. } => {
                    "~/.ssh mounted read-only into push containers".to_string()
                }
            };
            if grants.ssh.as_ref() != Some(&grant) {
                grants.ssh_destinations.clear();
            }
            grants.ssh = Some(grant);
            Resolution::Satisfied(Some(text))
        };
        match &self.options.flags.ssh {
            Some(SshFlag::Deny) => {
                return Ok(Resolution::Declined(
                    "SSH keys denied by --prerequisite-ssh deny; push_branch will fail".into(),
                ))
            }
            Some(SshFlag::All) => return Ok(granted(grants, self.whole_ssh_dir())),
            Some(SshFlag::Key(path)) => {
                return Ok(match self.ssh_key_grant(path) {
                    Ok(grant) => granted(grants, grant),
                    Err(reason) => Resolution::Declined(reason),
                })
            }
            None => {}
        }
        let policy = self.options.policy.ssh.clone();
        if policy.use_ == CredentialUse::Deny {
            return Ok(Resolution::Unchanged);
        }
        let remembered = self.remembered();
        if remembered.ssh_dir {
            return Ok(granted(grants, self.whole_ssh_dir()));
        }
        if let Some(key) = &remembered.ssh_key {
            if let Ok(grant) = self.ssh_key_grant(key) {
                return Ok(granted(grants, grant));
            }
            frontend.write_message(warning(
                "remembered SSH key is no longer usable; select a key again",
            ));
        }
        if policy.use_ == CredentialUse::Allow {
            if policy.whole_dir {
                return Ok(granted(grants, self.whole_ssh_dir()));
            }
            if let (Some(key), false) = (&policy.key, policy.key_is_suggestion) {
                match self.ssh_key_grant(key) {
                    Ok(grant) => return Ok(granted(grants, grant)),
                    Err(reason) => frontend.write_message(warning(reason)),
                }
            }
            if policy.key.is_none() && !policy.whole_dir {
                frontend.write_message(warning(
                    "ssh.use: allow needs ssh.key (or an explicit whole-directory grant)",
                ));
            }
        }
        if !can_ask {
            return Ok(headless_declined(check, "SSH key"));
        }
        if !check.remediations.iter().any(|r| {
            matches!(
                r,
                Remediation::MountSshKey { .. } | Remediation::MountSshDir
            )
        }) {
            return Ok(Resolution::Unchanged);
        }
        let remote_display = self
            .remote_url(remote)
            .map(|u| u.redacted)
            .unwrap_or_else(|| remote.to_string());
        let prompt = crate::command::prompts::prerequisite_ssh(check, &remote_display);
        match frontend.ask_prerequisite_ssh(&prompt)? {
            SshChoice::Key { path, remember } => match self.ssh_key_grant(&path) {
                Ok(grant) => {
                    if remember {
                        if let SshGrant::Key { key_path, .. } = &grant {
                            let key = key_path.clone();
                            self.remember(
                                RememberedKind::Ssh,
                                RememberedGrant::SshKey { key },
                                frontend,
                            );
                        }
                    }
                    Ok(granted(grants, grant))
                }
                Err(reason) => {
                    frontend.write_message(warning(format!("{reason}; push_branch will fail")));
                    Ok(Resolution::Declined(reason))
                }
            },
            SshChoice::WholeDir => Ok(granted(grants, self.whole_ssh_dir())),
            SshChoice::Decline => {
                frontend.write_message(warning(
                    "continuing without SSH keys: push_branch will fail",
                ));
                Ok(Resolution::Declined(
                    "SSH keys declined; push_branch will fail".into(),
                ))
            }
        }
    }

    // ─── GitHub token ───────────────────────────────────────────────────────

    fn env_grant(&self, host: &str, var: &str) -> Option<TokenGrant> {
        self.resolver(host).env_token(var).map(|secret| TokenGrant {
            secret,
            source: TokenGrantSource::Env(var.to_string()),
            host: host.to_string(),
            acquire: None,
        })
    }

    fn resolve_token(
        &mut self,
        check: &PrerequisiteCheck,
        host: &str,
        grants: &mut PrerequisiteGrants,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
        can_ask: bool,
    ) -> Result<Resolution, CommandError> {
        let rejected = check
            .remediations
            .contains(&Remediation::ReplaceRejectedToken);
        if rejected {
            grants.token = None;
            grants.token_destinations.clear();
        }
        if grants.token.as_ref().is_some_and(|t| t.host == host)
            && self
                .remote_url(&self.decision_remote)
                .is_some_and(|u| grants.allows_token(&u.redacted))
        {
            return Ok(Resolution::Satisfied(None));
        }
        // A non-GitHub host or GHE: no token grant can help in this version.
        if check
            .remediations
            .iter()
            .any(|r| matches!(r, Remediation::AddOverlayHint { .. }))
        {
            return Ok(Resolution::Unchanged);
        }
        let granted = |grants: &mut PrerequisiteGrants, grant: TokenGrant| {
            let text = match &grant.source {
                TokenGrantSource::Env(var) => format!("token from {var} granted to teardown steps"),
                TokenGrantSource::Pasted => "pasted token granted to teardown steps".to_string(),
                TokenGrantSource::HostCommand(label) => {
                    format!("token from {label} granted to teardown steps")
                }
                TokenGrantSource::ApiSecret(name) => {
                    format!("token from secret {name} granted to teardown steps")
                }
                TokenGrantSource::GhAuthToken => {
                    "token from gh auth token granted to teardown steps".to_string()
                }
            };
            if grants
                .token
                .as_ref()
                .is_some_and(|old| old.host != grant.host || old.secret != grant.secret)
            {
                grants.token_destinations.clear();
            }
            grants.token = Some(grant);
            Resolution::Satisfied(Some(text))
        };
        if !rejected {
            match &self.options.flags.token {
                Some(TokenFlag::Deny) => {
                    return Ok(Resolution::Declined(
                        "GitHub token denied by --prerequisite-token deny".into(),
                    ))
                }
                Some(TokenFlag::Env(var)) => {
                    return Ok(match self.env_grant(host, var) {
                        Some(grant) => granted(grants, grant),
                        None => Resolution::Declined(format!(
                            "--prerequisite-token env:{var}: {var} is not set"
                        )),
                    })
                }
                Some(TokenFlag::Acquire) => {
                    let Some(acquire) = self.options.policy.github_token.acquire.clone() else {
                        return Ok(Resolution::Declined(
                        "--prerequisite-token acquire: no workflows.prerequisites.githubToken.acquire command is configured"
                            .into(),
                    ));
                    };
                    return Ok(
                        match self.acquire_token(&acquire, host, frontend, can_ask)? {
                            Some(grant) => granted(grants, grant),
                            None => Resolution::Declined(
                                "the configured token command did not produce a token".into(),
                            ),
                        },
                    );
                }
                Some(TokenFlag::Secret(name)) => {
                    return Err(CommandError::PrerequisiteFlagUnsupported {
                        flag: format!("--prerequisite-token secret:{name}"),
                    })
                }
                None => {}
            }
        }
        let policy = self.options.policy.github_token.clone();
        if policy.use_ == CredentialUse::Deny {
            return Ok(Resolution::Unchanged);
        }
        if let Some(var) = self.remembered().token_env.filter(|_| !rejected) {
            if policy.env_vars.contains(&var) {
                if let Some(grant) = self.env_grant(host, &var) {
                    return Ok(granted(grants, grant));
                }
            }
        }
        let resolver = self.resolver(host);
        let env_var = if rejected {
            None
        } else {
            resolver.env_token_name()
        };
        if !rejected && policy.use_ == CredentialUse::Allow && host == "github.com" {
            if let Some(var) = &env_var {
                if let Some(grant) = self.env_grant(host, var) {
                    return Ok(granted(grants, grant));
                }
            }
            if let Some(acquire) = policy.acquire.clone() {
                if let Some(grant) = self.acquire_token(&acquire, host, frontend, can_ask)? {
                    return Ok(granted(grants, grant));
                }
            }
        }
        if check.status == PrerequisiteStatus::Satisfied {
            // poll_ci only, through an authenticated host gh.
            return Ok(Resolution::Unchanged);
        }
        if !can_ask {
            return Ok(headless_declined(check, "GitHub token"));
        }
        if let Some(var) = env_var {
            let prompt = crate::command::prompts::prerequisite_token_consent(check, &var);
            match frontend.ask_prerequisite_token_consent(&prompt)? {
                TokenConsent::ThisRun => {
                    if let Some(grant) = self.env_grant(host, &var) {
                        return Ok(granted(grants, grant));
                    }
                }
                TokenConsent::Remember => {
                    if let Some(grant) = self.env_grant(host, &var) {
                        self.remember(
                            RememberedKind::GithubToken,
                            RememberedGrant::TokenEnv { var: var.clone() },
                            frontend,
                        );
                        return Ok(granted(grants, grant));
                    }
                }
                TokenConsent::Decline => {}
            }
        }
        let mut source_check = check.clone();
        source_check
            .remediations
            .retain(|r| !matches!(r, Remediation::UseEnvToken { .. }));
        if !source_check.remediations.iter().any(|r| {
            matches!(
                r,
                Remediation::PasteToken
                    | Remediation::RunGhAuthToken { .. }
                    | Remediation::RunGhAuthLogin { .. }
                    | Remediation::RunAcquireCommand { .. }
            )
        }) {
            source_check.remediations.insert(0, Remediation::PasteToken);
        }
        let prompt = crate::command::prompts::prerequisite_token_source(&source_check);
        for _ in 0..TOKEN_SOURCE_ATTEMPTS {
            let grant = match frontend.ask_prerequisite_token_source(&prompt)? {
                TokenSourceChoice::Paste => self.pasted_token(host, frontend)?,
                TokenSourceChoice::GhAuthToken => self.gh_auth_token(host, frontend)?,
                TokenSourceChoice::GhAuthLogin => self.gh_auth_login(host, frontend)?,
                TokenSourceChoice::Acquire => match policy.acquire.clone() {
                    Some(acquire) => self.acquire_token(&acquire, host, frontend, can_ask)?,
                    None => None,
                },
                TokenSourceChoice::Decline => {
                    frontend.write_message(warning(
                        "continuing without a GitHub token: create_pull_request will fail; \
                         poll_ci will fall back to unauthenticated, rate-limited requests",
                    ));
                    return Ok(Resolution::Declined(
                        "GitHub token declined; create_pull_request will fail".into(),
                    ));
                }
            };
            if let Some(grant) = grant {
                return Ok(granted(grants, grant));
            }
        }
        Ok(Resolution::Declined(
            "no GitHub token was obtained; create_pull_request will fail".into(),
        ))
    }

    fn pasted_token(
        &mut self,
        host: &str,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> Result<Option<TokenGrant>, CommandError> {
        let Some(secret) = frontend
            .ask_prerequisite_secret(&crate::command::prompts::prerequisite_token_paste())?
        else {
            return Ok(None);
        };
        let value = secret.expose().trim();
        if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            frontend.write_message(warning(
                "the pasted value is not a token (empty, or contains whitespace)",
            ));
            return Ok(None);
        }
        let secret = if value.len() == secret.expose().len() {
            secret
        } else {
            SecretString::new(value)
        };
        Ok(Some(TokenGrant {
            secret,
            source: TokenGrantSource::Pasted,
            host: host.to_string(),
            acquire: None,
        }))
    }

    /// Run a token command, attached to the terminal when a person is there.
    fn run_host_command(
        &self,
        command: &HostSecretCommand,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
        attached: bool,
    ) -> Option<SecretString> {
        let review = command.review();
        let result = if attached {
            frontend.with_terminal_released(&review.label, &mut || command.run())
        } else {
            command.run()
        };
        match result {
            Ok(output) => {
                if !output.stderr.trim().is_empty() {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!("{}: {}", review.label, output.stderr.trim()),
                    });
                }
                if output.extra_lines > 0 {
                    frontend.write_message(warning(format!(
                        "{} printed {} extra line(s); only the first non-empty line was used",
                        review.label, output.extra_lines
                    )));
                }
                output.secret
            }
            Err(e) => {
                frontend.write_message(warning(format!("{e}")));
                None
            }
        }
    }

    fn gh_auth_token(
        &mut self,
        host: &str,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> Result<Option<TokenGrant>, CommandError> {
        let command = match self.resolver(host).gh_auth_token_command(HostStdin::Tty) {
            Ok(command) => command,
            Err(e) => {
                frontend.write_message(warning(format!("{e}")));
                return Ok(None);
            }
        };
        Ok(self
            .run_host_command(&command, frontend, true)
            .map(|secret| TokenGrant {
                secret,
                source: TokenGrantSource::GhAuthToken,
                host: host.to_string(),
                acquire: None,
            }))
    }

    fn gh_auth_login(
        &mut self,
        host: &str,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> Result<Option<TokenGrant>, CommandError> {
        match self.resolver(host).gh_auth_login_command() {
            Ok(command) => {
                let _ = self.run_host_command(&command, frontend, true);
            }
            Err(e) => {
                frontend.write_message(warning(format!("{e}")));
                return Ok(None);
            }
        }
        self.gh_auth_token(host, frontend)
    }

    /// Run the configured `acquire` command. A repository-scoped command
    /// runs only after approval for its exact hash (stored in the trust
    /// store); a frontend with nobody at it never approves one.
    fn acquire_token(
        &mut self,
        acquire: &AcquirePolicy,
        host: &str,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
        can_ask: bool,
    ) -> Result<Option<TokenGrant>, CommandError> {
        let stdin = if can_ask {
            HostStdin::Tty
        } else {
            HostStdin::Null
        };
        let command = match self.resolver(host).acquire_command(
            acquire,
            stdin,
            &self.options.custom_agent_binaries,
        ) {
            Ok(command) => command,
            Err(e) => {
                frontend.write_message(warning(format!("token command not run: {e}")));
                return Ok(None);
            }
        };
        if command.resolved_path().starts_with(&self.options.git_root) {
            frontend.write_message(warning(
                "token commands must be installed outside the workflow repository",
            ));
            return Ok(None);
        }
        if acquire.scope == AcquireScope::Repo && !self.repo_acquire_approved() {
            if !can_ask {
                frontend.write_message(warning(
                    "the repository's token command (workflows.prerequisites.githubToken.acquire) \
                     is not approved; skipped. Approve it by running the workflow interactively.",
                ));
                return Ok(None);
            }
            let review = command.review();
            let prompt = crate::command::prompts::prerequisite_acquire_approval(&review);
            match frontend.confirm_prerequisite_host_command(&review, &prompt)? {
                TrustDecision::Approve => {
                    let recorded = self.trust_store().map(|mut store| {
                        store.approve(
                            self.acquire_trust_key(),
                            ACQUIRE_TRUST_NAME,
                            &command.sha256(),
                        )
                    });
                    if !matches!(recorded, Some(Ok(()))) {
                        frontend.write_message(warning(
                            "could not record the approval; you will be asked again next run",
                        ));
                    }
                }
                TrustDecision::Decline => {
                    frontend.write_message(warning(
                        "the repository's token command was not approved; skipped",
                    ));
                    return Ok(None);
                }
            }
        }
        Ok(self
            .run_host_command(&command, frontend, can_ask)
            .map(|secret| TokenGrant {
                secret,
                source: TokenGrantSource::HostCommand(command.review().label),
                host: host.to_string(),
                acquire: Some(acquire.clone()),
            }))
    }

    // ─── Later phases ───────────────────────────────────────────────────────

    /// The non-interactive re-check just before teardown: verifies granted
    /// env vars are still set and re-runs a `refresh: "beforeTeardown"`
    /// token command. Writes **one** warning when anything is not satisfied.
    pub fn recheck_before_teardown(
        &self,
        needs: &WorkflowPrerequisites,
        grants: &mut PrerequisiteGrants,
        redaction: &RedactionSet,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> WorkflowPrerequisiteReport {
        let preflight = self.build_preflight(
            needs.clone(),
            PrerequisitePhase::BeforeTeardown,
            false,
            Some(grants.clone()),
        );
        let report = preflight.recheck_before_teardown(
            grants,
            redaction,
            &self.options.custom_agent_binaries,
        );
        let gaps: Vec<String> = report
            .checks
            .iter()
            .filter(|c| c.status != PrerequisiteStatus::Satisfied)
            .map(|c| format!("{} ({})", c.prerequisite.label(), c.explanation))
            .collect();
        if !gaps.is_empty() {
            frontend.write_message(warning(format!(
                "workflow prerequisites before teardown: {} not satisfied: {}",
                gaps.len(),
                gaps.join("; ")
            )));
        }
        frontend.report_workflow_prerequisites_recheck(&report);
        report
    }

    /// Dynamic runs: recompute the needs from the generated workflow, drop
    /// grants nothing uses, warn about newly needed prerequisites that were
    /// not granted (and ask again when a person is there), and warn when a
    /// generated step's own overlays ask for a credential the user declined.
    pub fn narrow_after_generation(
        &mut self,
        workflow: &Workflow,
        prepared_grants: &mut PrerequisiteGrants,
        redaction: &RedactionSet,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> Result<(WorkflowPrerequisites, WorkflowPrerequisiteReport), CommandError> {
        let can_ask = frontend.can_ask_prerequisites();
        let needs = WorkflowPrerequisites::from_workflow(workflow);
        let destinations: std::collections::BTreeSet<_> = self
            .remotes_for(&needs)
            .into_values()
            .map(|u| u.redacted)
            .collect();
        let changed_ssh = !prepared_grants.ssh_destinations.is_empty()
            && !needs
                .needs
                .iter()
                .filter_map(|n| match &n.prerequisite {
                    WorkflowPrerequisite::PushAuth { remote }
                    | WorkflowPrerequisite::PushAuthSsh { remote, .. } => self.remote_url(remote),
                    _ => None,
                })
                .all(|u| prepared_grants.allows_ssh(&u.redacted));
        let changed_token = needs.needs.iter().any(|n| match &n.prerequisite {
            WorkflowPrerequisite::PushAuth { remote }
            | WorkflowPrerequisite::PushAuthHttps { remote, .. } => {
                self.remote_url(remote).is_some_and(|u| {
                    matches!(
                        u.kind,
                        crate::data::workflow_prerequisites::GitRemoteKind::Https { .. }
                    ) && !prepared_grants.allows_token(&u.redacted)
                })
            }
            _ => false,
        });
        if changed_ssh {
            self.options.flags.ssh = None;
            self.options.policy.ssh.use_ = CredentialUse::Ask;
        }
        if changed_token {
            self.options.flags.token = None;
            self.options.policy.github_token.use_ = CredentialUse::Ask;
        }
        prepared_grants
            .ssh_destinations
            .retain(|u| destinations.contains(u));
        prepared_grants
            .token_destinations
            .retain(|u| destinations.contains(u));
        if changed_ssh {
            prepared_grants.ssh = None;
        }
        if changed_token {
            prepared_grants.token = None;
        }
        let preflight = self.build_preflight(
            needs,
            PrerequisitePhase::Start,
            can_ask,
            Some(prepared_grants.clone()),
        );
        let mut report = preflight.run();
        let resolved = preflight.resolved_prerequisites(&report);
        for dropped in prepared_grants.retain_for(&resolved) {
            frontend.write_message(warning(match dropped {
                DroppedGrant::Identity => "granted but unused: git identity (not injected)",
                DroppedGrant::Ssh => "granted but unused: SSH key (not mounted)",
                DroppedGrant::Token => "granted but unused: GitHub token (not passed)",
            }));
            report.notes.push(
                match dropped {
                    DroppedGrant::Identity => "granted but unused: git identity (not injected)",
                    DroppedGrant::Ssh => "granted but unused: SSH key (not mounted)",
                    DroppedGrant::Token => "granted but unused: GitHub token (not passed)",
                }
                .to_string(),
            );
        }
        self.warn_generated_overlays(workflow, prepared_grants, frontend);
        if !report.needs_attention().is_empty() {
            for check in report.needs_attention() {
                frontend.write_message(warning(format!(
                    "the generated workflow needs {}, which was not granted: {}",
                    check.prerequisite.label(),
                    check.explanation
                )));
            }
        }
        let outcome = if can_ask && !report.needs_attention().is_empty() {
            self.decide(report, prepared_grants.clone(), redaction, frontend, true)?
        } else {
            let unresolved = report
                .needs_attention()
                .into_iter()
                .map(|c| c.prerequisite.clone())
                .collect();
            PrerequisiteOutcome {
                report,
                grants: prepared_grants.clone(),
                unresolved,
            }
        };
        // The need was assumed until now, so only an explicit `fail` stops
        // the run after the leader has designed it.
        if !outcome.unresolved.is_empty()
            && self.options.policy.unresolved_or(UnresolvedMode::Warn) == UnresolvedMode::Fail
        {
            return Err(CommandError::WorkflowPrerequisitesUnresolved {
                report: Box::new(outcome.report),
            });
        }
        *prepared_grants = outcome.grants;
        for secret in prepared_grants.redaction_secrets() {
            redaction.add(secret);
        }
        Ok((resolved, outcome.report))
    }

    fn warn_generated_overlays(
        &self,
        workflow: &Workflow,
        grants: &PrerequisiteGrants,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) {
        let token_vars = &self.options.policy.github_token.env_vars;
        let mut lists: Vec<(String, &Vec<String>)> = Vec::new();
        for step in &workflow.steps {
            if let Some(overlays) = &step.overlays {
                lists.push((format!("step '{}'", step.name), overlays));
            }
        }
        for (i, entry) in workflow.setup.iter().enumerate() {
            if let Some(overlays) = &entry.overlays {
                lists.push((format!("setup step {i}"), overlays));
            }
        }
        for (i, entry) in workflow.teardown.iter().enumerate() {
            if let Some(overlays) = &entry.overlays {
                lists.push((format!("teardown step {i}"), overlays));
            }
        }
        for (owner, overlays) in lists {
            for overlay in overlays {
                let trimmed = overlay.trim();
                if trimmed.starts_with("ssh(") && grants.ssh.is_none() {
                    frontend.write_message(warning(format!(
                        "generated workflow {owner} requests '{trimmed}', which mounts ~/.ssh; \
                         SSH keys were not granted to this run"
                    )));
                }
                if let Some(rest) = trimmed.strip_prefix("env(") {
                    let name = rest.trim_end_matches(')').trim();
                    if token_vars.iter().any(|v| v == name) && grants.token.is_none() {
                        frontend.write_message(warning(format!(
                            "generated workflow {owner} requests '{trimmed}', a GitHub token \
                             variable this run was not granted"
                        )));
                    }
                }
            }
        }
    }
}

fn warning(text: impl Into<String>) -> UserMessage {
    UserMessage {
        level: MessageLevel::Warning,
        text: text.into(),
    }
}

fn declined_identity() -> Resolution {
    Resolution::Declined("no git identity granted; commit_changes will fail".into())
}

/// The headless answer (`HeadlessDefaults::prerequisite_credential_use` is
/// always `Deny`) applied to a check that needed consent.
fn headless_declined(check: &PrerequisiteCheck, what: &str) -> Resolution {
    match check.status {
        PrerequisiteStatus::NeedsConsent => Resolution::Declined(format!(
            "{}; nobody to ask, so the {what} was not used (pass a --prerequisite-* flag or set \
             workflows.prerequisites in ~/.awman/config.json)",
            check.explanation
        )),
        _ => Resolution::Unchanged,
    }
}
