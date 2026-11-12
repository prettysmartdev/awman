//! `HostAgentPinger` — the only type in awman that may execute an agent
//! binary on the developer's host machine.
//!
//! `aspec/architecture/security.md` §Guidance forbids running a code
//! assistant on the host, with one sanctioned exception: the ready-check
//! ping for a first-party agent or the optional `[ping]` command of an
//! approved custom definition. Its purpose is to make the host agent rotate its own
//! credential (and, during `awman ready`, to prove it is installed and
//! authenticated). Before WI 0114 F-38 that exception was a pair of bare
//! `pub async fn`s callable from any module. It is now this one type, so the
//! audit question "who can spawn an agent on the host?" has a single answer:
//! whoever holds a `HostAgentPinger`. Today that is `ReadyEngine` and
//! `CredentialRefreshMonitor`, and nobody else. The monitor registers only
//! first-party credential specifications; custom pings run only during ready.
//!
//! What the ping does is unchanged and deliberately narrow: the prompt comes
//! only from the hardcoded [`GREETINGS`] table, the argv is fixed per agent
//! by `AgentDefinition::ping_command`. The `ResolvedAgent` argument proves
//! custom argv from `[ping]` passed the registry's approval check.
//! First-party pings pin a cheapest model where known. No `run.env` or
//! `auth.env` from a custom definition is applied to the host process.
//! The process runs in a dedicated empty 0700 directory OUTSIDE the
//! repository — never the repo cwd — so a real code assistant launched this
//! way cannot discover repository instructions or content, and a
//! repo-planted `./claude` cannot be picked up via a relative lookup
//! (INV-8, BLOCKING-2).

use crate::engine::agent::{AgentDefinition, ResolvedAgent};
use crate::engine::auth::credential::{HostRefreshAction, RefreshableCredentialSpec};

pub const GREETINGS: [&str; 50] = [
    "Hello",
    "Hi there",
    "Hey",
    "Greetings",
    "Good day",
    "Howdy",
    "Salutations",
    "How are you",
    "Good morning",
    "Good afternoon",
    "Good evening",
    "Hi",
    "Hey there",
    "Ahoy",
    "Yo",
    "Hello there",
    "Hiya",
    "How's it going",
    "How do you do",
    "Pleased to meet you",
    "Nice to meet you",
    "How are things",
    "What's new",
    "How have you been",
    "Welcome",
    "Aloha",
    "Bonjour",
    "Ciao",
    "Hola",
    "Namaste",
    "Howdy partner",
    "Top of the morning to you",
    "What's happening",
    "How goes it",
    "How's everything",
    "How's life",
    "Well hello",
    "Hey friend",
    "Good to see you",
    "Hello friend",
    "Greetings and salutations",
    "Hey buddy",
    "Sup",
    "What's up",
    "Long time no see",
    "Rise and shine",
    "How's your day going",
    "Hope you're doing well",
    "Great to hear from you",
    "Glad you're here",
];

pub fn select_random_greeting() -> &'static str {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    GREETINGS[(secs % GREETINGS.len() as u64) as usize]
}

/// Result of the sanctioned host-side agent ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAgentPingResult {
    /// The agent replied. The greeting and response are the two lines that
    /// the ready phase reports to its frontend.
    Ok {
        greeting: String,
        response: String,
    },
    /// The agent ran but exited unsuccessfully, usually because it is not
    /// authenticated.
    Error,
    NotInstalled,
    CouldNotRun,
    TimedOut,
}

/// Ready-only ping seam. Production uses the sanctioned `HostAgentPinger`;
/// tests inject a fixed result without starting an agent executable.
#[async_trait::async_trait]
pub trait ReadyHostPing: Send + Sync {
    async fn ping(&self, def: &ResolvedAgent) -> LocalAgentPingResult;
}

#[async_trait::async_trait]
impl ReadyHostPing for HostAgentPinger {
    async fn ping(&self, def: &ResolvedAgent) -> LocalAgentPingResult {
        HostAgentPinger::ping(self, def).await
    }
}

/// Result of trying to make the host agent rotate its credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRefreshOutcome {
    /// The credential expiry advanced after the sanctioned ping.
    Advanced { expires_at: std::time::SystemTime },
    /// The ping completed, but the host credential did not advance. The
    /// existing last-known-good credential remains usable until it expires.
    NotAdvanced { remediation: String },
    /// The sanctioned ping itself failed.
    PingFailed { result: LocalAgentPingResult },
}

const HOST_REFRESH_REMEDIATION: &str = "run `claude` on the host / check login";

/// The one type permitted to execute an agent binary on the host.
///
/// Stateless today, and deliberately so: it exists to make the sanctioned
/// exception a named capability that a type must hold rather than a free
/// function any module can reach for. `ReadyEngine` holds one;
/// `CredentialRefreshMonitor` holds one. A new holder is a security review.
#[derive(Debug, Clone, Default)]
pub struct HostAgentPinger {
    env: Option<crate::data::config::env::EnvSnapshot>,
}

impl HostAgentPinger {
    pub fn new() -> Self {
        Self::with_env(crate::engine::host_cli::process_env())
    }
    pub fn with_env(env: crate::data::config::env::EnvSnapshot) -> Self {
        Self { env: Some(env) }
    }

    /// Build the fixed `(command, argv)` for the sanctioned host ping. Pure
    /// and side-effect free so it can be asserted in tests (INV-8). The
    /// prompt is drawn only from the hardcoded [`GREETINGS`] table and, for
    /// first-party agents where a cheapest-model flag is known, the cheapest
    /// model is pinned so the ready-check refresh never bills a premium model
    /// (security.md §Guidance).
    ///
    /// `None` when the definition declares no host ping. There is no
    /// catch-all: an unknown name never reaches here, because
    /// `AgentRegistry::resolve` already failed with `UnknownAgent`, so no argv
    /// is ever invented for an agent.
    pub(crate) fn ping_command(
        def: &AgentDefinition,
        greeting: &str,
    ) -> Option<(String, Vec<String>)> {
        def.ping_command(greeting)
    }

    /// The one host-side agent execution permitted to awman.
    ///
    /// Keep this invocation deliberately narrow: the prompt comes only from
    /// the hardcoded [`GREETINGS`] table, the command arguments are fixed per
    /// agent, and — crucially — the process runs in a dedicated empty
    /// directory OUTSIDE the repository (never the repo cwd), so a real code
    /// assistant launched this way cannot discover repository instructions or
    /// content, and a repo-planted `./claude` cannot be picked up via a
    /// relative lookup (INV-8, BLOCKING-2). Both `awman ready` and the
    /// credential refresh monitor reach the host through this method.
    pub async fn ping(&self, def: &ResolvedAgent) -> LocalAgentPingResult {
        self.ping_with_timeout(def, std::time::Duration::from_secs(60))
            .await
    }

    async fn ping_with_timeout(
        &self,
        def: &ResolvedAgent,
        timeout: std::time::Duration,
    ) -> LocalAgentPingResult {
        let greeting = select_random_greeting();
        // A definition without a host ping spawns nothing; callers check
        // `def.ping_argv.is_none()` first and report the step `Skipped`.
        let Some((cmd, args)) = Self::ping_command(def, greeting) else {
            return LocalAgentPingResult::CouldNotRun;
        };

        // Run in a fresh empty 0700 directory so the host agent inherits no
        // repository as its working directory. A dedicated TempDir is
        // required: a shared system temp directory is not empty or private.
        let Ok(scratch) = tempfile::Builder::new()
            .prefix("awman-ready-ping-")
            .tempdir()
        else {
            return LocalAgentPingResult::CouldNotRun;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))
                .is_err()
            {
                return LocalAgentPingResult::CouldNotRun;
            }
        }

        let snapshot = self
            .env
            .clone()
            .unwrap_or_else(crate::engine::host_cli::process_env);
        let mut command =
            tokio::process::Command::new(crate::engine::host_cli::program_in(&snapshot, &cmd));
        command.env_clear().envs(snapshot.spawn_env());
        command
            .args(&args)
            .current_dir(scratch.path())
            .kill_on_drop(true);
        match tokio::time::timeout(timeout, command.output()).await {
            Err(_) => LocalAgentPingResult::TimedOut,
            Ok(result) => match result {
                Ok(output) if output.status.success() => {
                    let response = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    LocalAgentPingResult::Ok {
                        greeting: greeting.to_string(),
                        response,
                    }
                }
                Ok(_) => LocalAgentPingResult::Error,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    LocalAgentPingResult::NotInstalled
                }
                Err(_) => LocalAgentPingResult::CouldNotRun,
            },
        }
    }

    /// Run a descriptor's host refresh action and verify that its
    /// credential's expiry actually advanced. A stale or unreadable
    /// post-refresh credential is reported as a warning outcome so callers
    /// can retain the last-known-good file; it is never promoted to an
    /// engine error.
    pub async fn refresh_credential(
        &self,
        spec: &RefreshableCredentialSpec,
        binding: &crate::engine::auth::credential::CredentialBinding,
    ) -> HostRefreshOutcome {
        let source = binding.source_for(spec);
        let before = match (spec.read)(&source) {
            Ok(snapshot) => (spec.expiry)(&snapshot),
            Err(reason) => {
                tracing::warn!(
                    agent = spec.agent,
                    reason = %reason,
                    "could not read host credential before refresh"
                );
                return HostRefreshOutcome::NotAdvanced {
                    remediation: HOST_REFRESH_REMEDIATION.to_string(),
                };
            }
        };

        let ping_result = match (spec.host_refresh)() {
            HostRefreshAction::ReadyCheckPing { agent } => {
                // Credential refresh is first-party only: a refreshable spec
                // exists for no custom agent, so the monitor never pings one.
                if !crate::engine::agent::first_party::is_first_party(agent) {
                    return HostRefreshOutcome::PingFailed {
                        result: LocalAgentPingResult::CouldNotRun,
                    };
                }
                let Ok(name) = crate::data::session::AgentName::new(agent) else {
                    return HostRefreshOutcome::PingFailed {
                        result: LocalAgentPingResult::CouldNotRun,
                    };
                };
                let Ok(def) = crate::engine::agent::AgentRegistry::resolve(
                    &self
                        .env
                        .clone()
                        .unwrap_or_else(crate::engine::host_cli::process_env),
                    &name,
                    None,
                ) else {
                    return HostRefreshOutcome::PingFailed {
                        result: LocalAgentPingResult::CouldNotRun,
                    };
                };
                self.ping(&def).await
            }
        };
        if !matches!(&ping_result, LocalAgentPingResult::Ok { .. }) {
            return HostRefreshOutcome::PingFailed {
                result: ping_result,
            };
        }

        let after = match (spec.read)(&source) {
            Ok(snapshot) => (spec.expiry)(&snapshot),
            Err(reason) => {
                tracing::warn!(
                    agent = spec.agent,
                    reason = %reason,
                    "could not read host credential after refresh"
                );
                return HostRefreshOutcome::NotAdvanced {
                    remediation: HOST_REFRESH_REMEDIATION.to_string(),
                };
            }
        };

        match (before, after) {
            (Some(before), Some(after)) if after > before => {
                HostRefreshOutcome::Advanced { expires_at: after }
            }
            _ => {
                tracing::warn!(
                    agent = spec.agent,
                    remediation = HOST_REFRESH_REMEDIATION,
                    "host credential expiry did not advance after refresh"
                );
                HostRefreshOutcome::NotAdvanced {
                    remediation: HOST_REFRESH_REMEDIATION.to_string(),
                }
            }
        }
    }
}
