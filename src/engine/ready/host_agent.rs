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

#[cfg(test)]
use crate::engine::agent::first_party::first_party;
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
pub struct HostAgentPinger;

impl HostAgentPinger {
    pub fn new() -> Self {
        Self
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

        let mut command = tokio::process::Command::new(&cmd);
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
                    &crate::data::config::env::Env::from_process(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// INV-8 / BLOCKING-2: the Claude host ping pins the cheapest model,
    /// carries the greeting drawn from the hardcoded table, and takes no other
    /// argument (no user input, no repo content). The greeting is the only
    /// variable part.
    #[test]
    fn ping_command_for_claude_pins_cheapest_model_and_only_the_greeting() {
        let def = first_party("claude").unwrap();
        let greeting = select_random_greeting();
        let (cmd, args) = HostAgentPinger::ping_command(&def, greeting).unwrap();
        assert_eq!(cmd, "claude");
        assert_eq!(args, vec!["--model", "haiku", "--print", greeting]);
        // Every argument except the greeting is a fixed literal, and the
        // greeting itself is drawn only from the hardcoded table.
        assert!(
            GREETINGS.contains(&args.last().unwrap().as_str()),
            "the ping's only variable argument must come from GREETINGS"
        );
    }

    /// A definition without a `[ping]` produces no argv at all, and `ping`
    /// returns without spawning anything. There is no catch-all argv.
    #[tokio::test]
    async fn a_definition_without_a_ping_has_no_command_and_spawns_nothing() {
        let mut def = first_party("cline").unwrap();
        def.ping_argv = None;
        assert_eq!(HostAgentPinger::ping_command(&def, "Hello"), None);
        assert_eq!(
            HostAgentPinger::new()
                .ping(&ResolvedAgent::for_test(def))
                .await,
            LocalAgentPingResult::CouldNotRun
        );
    }

    /// The ping is reachable only through a `HostAgentPinger`. There is no
    /// free function left that spawns an agent on the host — that is the
    /// whole point of F-38, and `security.md` §Guidance names this type.
    #[test]
    fn a_pinger_is_the_only_handle_and_is_cheap_to_construct() {
        let a = HostAgentPinger::new();
        let b = a.clone();
        let _ = (a, b, HostAgentPinger);
    }
}

#[cfg(all(test, unix))]
mod custom_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn host_ping_is_time_bounded() {
        let mut def = crate::engine::agent::custom::crush_fixture();
        def.ping_argv = Some(vec!["/bin/sleep".into(), "60".into()]);
        // sleep receives a greeting too, so use a script that deliberately ignores argv.
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("sleep-ping");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        def.ping_argv = Some(vec![script.to_string_lossy().into_owned()]);
        assert_eq!(
            HostAgentPinger::new()
                .ping_with_timeout(
                    &ResolvedAgent::for_test(def),
                    std::time::Duration::from_millis(25)
                )
                .await,
            LocalAgentPingResult::TimedOut
        );
    }

    #[tokio::test]
    async fn custom_ping_uses_only_argv_and_a_fresh_private_empty_directory() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("probe");
        std::fs::write(&script, "#!/bin/sh\n[ -z \"$(ls -A .)\" ] || exit 1\n[ \"$(stat -c %a . 2>/dev/null || stat -f %Lp .)\" = 700 ] || exit 2\n[ -z \"$CUSTOM_AGENT_STATIC_TEST\" ] || exit 3\n[ -z \"$CUSTOM_AGENT_AUTH_TEST\" ] || exit 4\n[ \"$#\" = 2 ] || exit 5\n[ \"$1\" = '--ping' ] || exit 6\nprintf '%s\\n' \"$2\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut def = crate::engine::agent::custom::crush_fixture();
        def.ping_argv = Some(vec![script.to_string_lossy().into(), "--ping".into()]);
        def.static_env = vec![(
            "CUSTOM_AGENT_STATIC_TEST".into(),
            "must not be applied".into(),
        )];
        def.auth_env = vec!["CUSTOM_AGENT_AUTH_TEST".into()];
        // A value is available where container auth forwarding would read it
        // (the daemon env overlay); the host ping must still not receive it.
        crate::data::config::env::update_daemon_overlay(|overlay| {
            overlay.insert(
                "CUSTOM_AGENT_AUTH_TEST".into(),
                "sentinel-must-not-reach-ping".into(),
            );
        });
        let LocalAgentPingResult::Ok { greeting, response } = HostAgentPinger::new()
            .ping(&ResolvedAgent::for_test(def))
            .await
        else {
            panic!("probe failed")
        };
        crate::data::config::env::update_daemon_overlay(|overlay| {
            overlay.remove("CUSTOM_AGENT_AUTH_TEST");
        });
        assert!(GREETINGS.contains(&greeting.as_str()));
        assert_eq!(response, greeting);
    }
}
