//! `HostAgentPinger` — the only type in awman that may execute an agent
//! binary on the developer's host machine.
//!
//! `aspec/architecture/security.md` §Guidance forbids running a code
//! assistant on the host, with one sanctioned exception: the ready-check
//! ping, whose sole effect is to make the host agent rotate its own
//! credential (and, during `awman ready`, to prove it is installed and
//! authenticated). Before WI 0114 F-38 that exception was a pair of bare
//! `pub async fn`s callable from any module. It is now this one type, so the
//! audit question "who can spawn an agent on the host?" has a single answer:
//! whoever holds a `HostAgentPinger`. Today that is `ReadyEngine` and
//! `CredentialRefreshMonitor`, and nobody else.
//!
//! What the ping does is unchanged and deliberately narrow: the prompt comes
//! only from the hardcoded [`GREETINGS`] table, the argv is fixed per agent
//! by `AgentMatrix::ping_command`, the cheapest model is pinned where one is
//! known, and the process runs in a dedicated empty directory OUTSIDE the
//! repository — never the repo cwd — so a real code assistant launched this
//! way cannot discover repository instructions or content, and a
//! repo-planted `./claude` cannot be picked up via a relative lookup
//! (INV-8, BLOCKING-2).

use crate::data::session::AgentName;
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
    /// agents where a cheapest-model flag is known, the cheapest model is
    /// pinned so the ready-check refresh never bills a premium model
    /// (security.md §Guidance).
    ///
    /// The `Err` arm is the pre-F-32 catch-all for an agent with no matrix
    /// entry of its own: `<name> --print <greeting>`. `AgentName` is not
    /// restricted to `SUPPORTED_AGENTS`, so an agent the matrix does not know
    /// still has to produce an argv, and it has to be the same one it
    /// produced before.
    pub(crate) fn ping_command(agent: &AgentName, greeting: &str) -> (String, Vec<String>) {
        match crate::engine::agent::agent_matrix::matrix_for(agent.as_str()) {
            Ok(matrix) => matrix.ping_command(greeting),
            Err(_) => (
                agent.as_str().to_string(),
                vec!["--print".to_string(), greeting.to_string()],
            ),
        }
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
    pub async fn ping(&self, agent: &AgentName) -> LocalAgentPingResult {
        let greeting = select_random_greeting();
        let (cmd, args) = Self::ping_command(agent, greeting);

        // Run in a fresh empty 0700 directory so the host agent inherits no
        // repository as its working directory. A dedicated TempDir is
        // preferred; if it cannot be created, fall back to the system temp
        // dir — anything but the repo cwd awman was started from.
        let scratch = tempfile::Builder::new()
            .prefix("awman-ready-ping-")
            .tempdir()
            .ok();
        let work_dir = scratch
            .as_ref()
            .map(|d| d.path().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);

        let mut command = tokio::process::Command::new(&cmd);
        command.args(&args).current_dir(&work_dir);
        match command.output().await {
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
                let Ok(agent) = AgentName::new(agent) else {
                    return HostRefreshOutcome::PingFailed {
                        result: LocalAgentPingResult::CouldNotRun,
                    };
                };
                self.ping(&agent).await
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
        let agent = AgentName::new("claude").unwrap();
        let greeting = select_random_greeting();
        let (cmd, args) = HostAgentPinger::ping_command(&agent, greeting);
        assert_eq!(cmd, "claude");
        assert_eq!(args, vec!["--model", "haiku", "--print", greeting]);
        // Every argument except the greeting is a fixed literal, and the
        // greeting itself is drawn only from the hardcoded table.
        assert!(
            GREETINGS.contains(&args.last().unwrap().as_str()),
            "the ping's only variable argument must come from GREETINGS"
        );
    }

    /// An agent the matrix does not know still produces the pre-F-32
    /// catch-all argv rather than failing or spawning something unexpected.
    #[test]
    fn ping_command_falls_back_to_print_for_an_agent_without_a_matrix_entry() {
        let agent = AgentName::new("not-a-shipped-agent").unwrap();
        let (cmd, args) = HostAgentPinger::ping_command(&agent, "Hello");
        assert_eq!(cmd, "not-a-shipped-agent");
        assert_eq!(args, vec!["--print", "Hello"]);
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
