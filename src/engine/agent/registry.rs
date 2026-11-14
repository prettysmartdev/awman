//! `AgentRegistry` — the one lookup from an agent name to its
//! `AgentDefinition`.
//!
//! Precedence: first-party → repo `<git_root>/.awman/agents/<name>.toml` →
//! global `<data_home>/agents/<name>.toml`. Callers resolve once per launch
//! and pass the `Arc` down, so a definition edited mid-launch can never
//! produce a mixed view.
//!

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::definition::{AgentDefinition, AgentOrigin};
use super::first_party::{first_party, FIRST_PARTY_AGENTS};
use crate::data::config::agent_definition::{
    load_agent_definition_with_scope, validate_agent_file_stem,
};
use crate::data::config::env::EnvSnapshot;
use crate::data::fs::agent_dirs::AgentDirs;
use crate::data::fs::agent_dirs::{DefinitionScope, SkippedDefinition};
use crate::data::fs::agent_trust::{TrustKey, TrustStore};
use crate::data::session::{AgentName, Session};
use crate::engine::error::EngineError;

/// Agents that used to be first-party and are now example custom
/// definitions. Naming one without a definition earns a migration hint.
const FORMER_FIRST_PARTY: &[&str] = &["crush", "maki"];

/// A resolved definition, shared by everything one launch prepares.
#[derive(Debug, Clone)]
pub struct ResolvedAgent(Arc<AgentDefinition>);

impl ResolvedAgent {
    pub fn definition(&self) -> Arc<AgentDefinition> {
        self.0.clone()
    }
}

/// Parsed data for review. This cannot be passed to build, render or host-ping sinks.
///
/// ```compile_fail
/// use awman::engine::{agent::registry::UntrustedAgent, ready::host_agent::HostAgentPinger};
/// fn unapproved_ping(pinger: &HostAgentPinger, definition: &UntrustedAgent) {
///     let _ = pinger.ping(definition);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct UntrustedAgent(Arc<AgentDefinition>);

impl UntrustedAgent {
    pub fn definition(&self) -> &AgentDefinition {
        &self.0
    }

    /// Include this in pre-approval review without rendering unapproved content.
    /// The trusted preview subsequently identifies exact differences and ownership.
    pub fn existing_dockerfile_notice(
        &self,
        paths: &crate::data::RepoDockerfilePaths,
    ) -> Option<String> {
        let path = paths.agent_dockerfile(&self.0.name);
        (self.0.is_custom() && path.exists()).then(|| format!(
            "an existing {} may be used instead of the definition's recipe; review that file too. Remove its header line to take ownership, or delete it to regenerate after approval",
            path.display()
        ))
    }
}

impl std::ops::Deref for ResolvedAgent {
    type Target = AgentDefinition;
    fn deref(&self) -> &AgentDefinition {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomListing {
    pub name: String,
    pub scope: DefinitionScope,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentListing {
    /// `FIRST_PARTY_AGENTS` in table order.
    pub first_party: Vec<String>,
    /// Repo first, then global; a name shadowed by repo is listed once (repo).
    pub custom: Vec<CustomListing>,
    pub skipped: Vec<SkippedDefinition>,
}

impl AgentListing {
    /// First-party then custom names, de-duplicated.
    pub fn names(&self) -> Vec<String> {
        let mut names = self.first_party.clone();
        for c in &self.custom {
            if !names.contains(&c.name) {
                names.push(c.name.clone());
            }
        }
        names
    }

    pub fn contains(&self, name: &str) -> bool {
        self.first_party.iter().any(|n| n == name) || self.custom.iter().any(|c| c.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustStatus {
    FirstParty,
    NotRequired,
    Approved,
    Pending,
}

pub struct AgentRegistry;

impl AgentRegistry {
    /// Resolve `name` to its definition. Fail-closed: a custom definition
    /// that needs approval and has none is `AgentDefinitionUntrusted`; an
    /// invalid repo file is `AgentDefinitionInvalid` and never falls through
    /// to global. Re-reads on every call; no cache.
    pub fn resolve(
        env: &EnvSnapshot,
        name: &AgentName,
        git_root: Option<&Path>,
    ) -> Result<ResolvedAgent, EngineError> {
        let def = Self::load_untrusted(env, name, git_root)?;
        let def = def.definition();
        if Self::trust_status(env, def, git_root)? == TrustStatus::Pending {
            if let AgentOrigin::Custom { path, sha256, .. } = &def.origin {
                return Err(EngineError::AgentDefinitionUntrusted {
                    name: def.name.clone(),
                    path: path.clone(),
                    sha256: sha256.clone(),
                });
            }
        }
        Ok(ResolvedAgent(Arc::new(def.clone())))
    }

    /// Resolve using the invoker's definition root, including in worktree sessions.
    pub fn resolve_for_session(
        session: &Session,
        name: &AgentName,
    ) -> Result<ResolvedAgent, EngineError> {
        Self::resolve(session.env(), name, Some(session.definition_root()))
    }

    /// Same lookup without the trust check. Only the command layer's
    /// review/approval flow and `awman ready`'s definition step may call it;
    /// it must never feed a build or launch.
    pub fn load_untrusted(
        env: &EnvSnapshot,
        name: &AgentName,
        git_root: Option<&Path>,
    ) -> Result<UntrustedAgent, EngineError> {
        if let Some(def) = first_party(name.as_str()) {
            return Ok(UntrustedAgent(Arc::new(def)));
        }
        if validate_agent_file_stem(name.as_str()).is_err() {
            return Err(Self::unknown(env, name.as_str(), git_root, false));
        }
        let dirs = AgentDirs::from_env(env, git_root.map(Path::to_path_buf))?;
        for scope in [DefinitionScope::Repo, DefinitionScope::Global] {
            let Some(path) = dirs.definition_path(scope, name.as_str()) else {
                continue;
            };
            match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(EngineError::AgentDefinitionInvalid {
                        path,
                        problems: vec![format!("{e} [scope: {}]", scope.as_str())],
                    });
                }
                Ok(_) => {}
            }
            // A requested broken or unsafe repo file must never fall back to global.
            let dir = path.parent().unwrap();
            let canonical = std::fs::canonicalize(&path);
            let canonical_dir = std::fs::canonicalize(dir);
            if let (Ok(target), Ok(parent)) = (&canonical, &canonical_dir) {
                if !target.starts_with(parent) {
                    return Err(EngineError::AgentDefinitionInvalid {
                        path: path.clone(),
                        problems: vec![format!(
                            "symlink points outside {} [scope: {}]",
                            dir.display(),
                            scope.as_str()
                        )],
                    });
                }
            }
            let loaded = load_agent_definition_with_scope(&path, scope)?;
            return Ok(UntrustedAgent(Arc::new(super::custom::convert(
                &loaded,
                name.as_str(),
                scope,
            ))));
        }
        Err(Self::unknown(env, name.as_str(), git_root, false))
    }

    /// Every agent available here. Never enforces trust, never parses files.
    pub fn list(env: &EnvSnapshot, git_root: Option<&Path>) -> AgentListing {
        let mut listing = AgentListing {
            first_party: FIRST_PARTY_AGENTS.iter().map(|s| s.to_string()).collect(),
            custom: Vec::new(),
            skipped: Vec::new(),
        };
        match AgentDirs::from_env(env, git_root.map(Path::to_path_buf)) {
            Ok(dirs) => {
                let discovery = dirs.discover();
                listing.skipped = discovery.skipped;
                for found in discovery.found {
                    if !listing.contains(&found.name) {
                        listing.custom.push(CustomListing {
                            name: found.name,
                            scope: found.scope,
                            path: found.path,
                        });
                    }
                }
            }
            Err(e) => listing.skipped.push(SkippedDefinition {
                path: git_root.unwrap_or(Path::new(".")).into(),
                reason: e.to_string(),
            }),
        }
        listing
    }

    /// Custom && (scope == Repo || declares a host ping).
    pub fn needs_approval(def: &AgentDefinition) -> bool {
        match def.scope() {
            None => false,
            Some(DefinitionScope::Repo) => true,
            Some(DefinitionScope::Global) => def.ping_argv.is_some(),
        }
    }

    pub fn trust_status(
        env: &EnvSnapshot,
        def: &AgentDefinition,
        git_root: Option<&Path>,
    ) -> Result<TrustStatus, EngineError> {
        if !def.is_custom() {
            return Ok(TrustStatus::FirstParty);
        }
        if !Self::needs_approval(def) {
            return Ok(TrustStatus::NotRequired);
        }
        let store = TrustStore::load(&crate::data::fs::data_paths::data_home(env)?);
        if let Some(warning) = &store.load_warning {
            tracing::warn!("{warning}");
        }
        let AgentOrigin::Custom { sha256, .. } = &def.origin else {
            unreachable!()
        };
        Ok(if store.is_trusted(&trust_key(def, git_root)?, sha256) {
            TrustStatus::Approved
        } else {
            TrustStatus::Pending
        })
    }

    /// Record approval for `def`'s current hash.
    pub fn record_approval(
        env: &EnvSnapshot,
        def: &AgentDefinition,
        git_root: Option<&Path>,
    ) -> Result<(), EngineError> {
        if !def.is_custom() {
            return Ok(());
        }
        let AgentOrigin::Custom { sha256, .. } = &def.origin else {
            unreachable!()
        };
        let mut store = TrustStore::load(&crate::data::fs::data_paths::data_home(env)?);
        if let Some(warning) = &store.load_warning {
            tracing::warn!("{warning}");
        }
        store.approve(trust_key(def, git_root)?, &def.name, sha256)?;
        Ok(())
    }

    /// Build the `UnknownAgent` error for `name` with the listing and, for a
    /// former built-in, the migration hint.
    pub fn unknown(
        env: &EnvSnapshot,
        name: &str,
        git_root: Option<&Path>,
        runtime_is_sandbox: bool,
    ) -> EngineError {
        let listing = Self::list(env, git_root);
        let mut notes = Vec::new();
        if FORMER_FIRST_PARTY.contains(&name) {
            notes.push(format!(
                "'{name}' is no longer built in — recreate it with: awman new agent {name} --from {name}"
            ));
        }
        if runtime_is_sandbox {
            notes.push(
                "(custom agents run on runtime.kind docker, apple or builtin; Docker Sandboxes \
                 support is not available for them yet)"
                    .to_string(),
            );
        }
        EngineError::UnknownAgent {
            name: name.to_string(),
            first_party: listing.first_party,
            custom: listing
                .custom
                .iter()
                .map(|c| format!("{} ({})", c.name, c.scope.as_str()))
                .collect(),
            notes,
        }
    }
}

fn trust_key(def: &AgentDefinition, git_root: Option<&Path>) -> Result<TrustKey, EngineError> {
    match &def.origin {
        AgentOrigin::Custom {
            scope: DefinitionScope::Repo,
            path,
            ..
        } => {
            let root = git_root.ok_or_else(|| EngineError::AgentDefinitionInvalid {
                path: path.clone(),
                problems: vec!["repo definition requires a git root for approval".into()],
            })?;
            Ok(TrustKey::Repo {
                git_root: root.into(),
                name: def.name.clone(),
            })
        }
        AgentOrigin::Custom { path, .. } => Ok(TrustKey::Global { path: path.clone() }),
        AgentOrigin::FirstParty => unreachable!("first-party agents need no trust key"),
    }
}
