//! Credential registration for the Docker Sandbox runtime (contract §7).
//!
//! Maps awman's credential env-var names to `sbx` service names and stores
//! each value with `sbx secret set <service> --sandbox <name> --force`,
//! piping the value via **stdin** so it never appears in argv (process
//! listings), logs, the apply document or a kit file.
//!
//! All registration is **sandbox-scoped and happens at agent-launch time**,
//! after the sandbox exists — never at `awman ready`, never globally. Scoped
//! secrets take precedence over global ones and take effect in an existing
//! sandbox without a restart; `sbx rm` deletes them with the sandbox.
//!
//! Wording rule: awman may say a secret **was registered** or **is stored**.
//! It never says the agent is authenticated or ready — storing a secret is
//! not proof that a composed kit is approved to use it, nor that the provider
//! accepts it. The agent's own exit status decides the task result.

use crate::data::message::UserMessageSink;
use crate::engine::agent::first_party::first_party;
use crate::engine::container::options::{EnvLiteral, EnvVar};
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::cli;
use crate::engine::sandbox::dsbx::spawn::SbxCommand;

/// Re-export the shared service→credential mapping so the dsbx module (and its
/// tests via `use super::*`) can use it without a long path. The canonical
/// definition lives in [`crate::engine::auth::service_for_credential`].
pub(super) use crate::engine::auth::service_for_credential;

/// Allowlist of provider auth env vars accepted via `env(VAR)` overlays for
/// launch-time registration, read from the first-party table
/// (`AgentDefinition::sandbox_auth_env_vars`).
pub(super) fn supported_auth_env_vars(agent: &str) -> Vec<String> {
    first_party(agent)
        .map(|d| d.sandbox_auth_env_vars)
        .unwrap_or_default()
}

/// Heuristic: does this env-var name look like it carries a secret? Such a
/// variable is never passed to the sandbox session with `-e`.
pub(super) fn is_credential_like(key: &str) -> bool {
    if service_for_credential(key).is_some() {
        return true;
    }
    let upper = key.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "API_KEY",
        "APIKEY",
        "CREDENTIAL",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

/// Host lookups the registration needs, injectable for tests.
pub(super) struct CredentialLookups<'a> {
    /// Value of a host environment variable named by an `env(VAR)` overlay.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Whether a secret for `service` is already stored for this sandbox or
    /// globally (contract 5.12).
    pub stored: &'a dyn Fn(&str) -> bool,
}

/// Register this launch's credentials for `sandbox`, then — only when
/// nothing was registered — probe whether a secret is already stored, and
/// warn when awman supplied none. Never claims authentication.
pub(super) fn register_launch_credentials(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
    sandbox: &str,
    overlays: (&[EnvVar], &[EnvLiteral]),
    keychain: &[(String, String)],
    lookups: &CredentialLookups<'_>,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    let mut registered = register_env_overlays(snapshot, agent, sandbox, overlays, lookups, sink)?;
    for service in register_keychain_credentials(snapshot, keychain, sandbox, sink)? {
        if !registered.contains(&service) {
            registered.push(service);
        }
    }
    let supported = supported_auth_env_vars(agent);
    if supported.is_empty() || !registered.is_empty() {
        return Ok(());
    }
    let mut services: Vec<&'static str> = Vec::new();
    for var in &supported {
        if let Some(service) = service_for_credential(var) {
            if !services.contains(&service) {
                services.push(service);
            }
        }
    }
    let stored: Vec<&str> = services
        .iter()
        .copied()
        .filter(|service| (lookups.stored)(service))
        .collect();
    if stored.is_empty() {
        let service = services.first().copied().unwrap_or("<service>");
        sink.write_message(warning(format!(
            "sbx: awman supplied no credential for '{agent}': no env(...) overlay named a \
             supported variable ({}), and no '{}' secret is stored for sandbox '{sandbox}' or \
             globally. Store one with `sbx secret set {service} --sandbox {sandbox}`, or \
             complete the agent's own login inside the sandbox. Launching anyway; the agent's \
             exit status decides the result.",
            supported.join(", "),
            services.join("', '"),
        )));
    } else {
        sink.write_message(info(format!(
            "sbx: no credential was registered this launch; a '{}' secret is already stored in \
             Docker Sandboxes. awman does not check that the agent can use it.",
            stored.join("', '")
        )));
    }
    Ok(())
}

/// Register `env(VAR)` overlays for the agent's supported auth variables.
/// Returns the services registered.
fn register_env_overlays(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
    sandbox: &str,
    overlays: (&[EnvVar], &[EnvLiteral]),
    lookups: &CredentialLookups<'_>,
    sink: &mut dyn UserMessageSink,
) -> Result<Vec<&'static str>, EngineError> {
    let (env_passthrough, env_literal) = overlays;
    let supported = supported_auth_env_vars(agent);
    let mut registered: Vec<&'static str> = Vec::new();

    for var in env_passthrough {
        let key = var.0.as_str();
        if supported.iter().any(|s| s == key) {
            let Some(service) = service_for_credential(key) else {
                warn_unsupported_dropped(agent, key, &supported, sink);
                continue;
            };
            if registered.contains(&service) {
                continue; // e.g. both GH_TOKEN and GITHUB_TOKEN requested
            }
            match (lookups.env)(key) {
                Some(value) if !value.is_empty() => {
                    set_secret_in(snapshot, service, &value, sandbox, sink)?;
                    registered.push(service);
                }
                _ => {
                    sink.write_message(warning(format!(
                        "sbx: env({key}) was requested but {key} is not set in the \
                         host environment; no '{service}' secret was registered."
                    )));
                }
            }
        } else if is_credential_like(key) {
            warn_unsupported_dropped(agent, key, &supported, sink);
        }
        // Non-credential vars reach the agent session as `sbx run -e KEY`.
    }

    for lit in env_literal {
        if is_credential_like(&lit.key) {
            sink.write_message(warning(format!(
                "sbx: credential-class env literal '{}' was withheld from the sandbox \
                 session. Pass it as an env({}) overlay to register it as a sandbox-scoped \
                 secret, or store it with `sbx secret set`.",
                lit.key, lit.key
            )));
        }
    }
    Ok(registered)
}

fn warn_unsupported_dropped(
    agent: &str,
    key: &str,
    supported: &[String],
    sink: &mut dyn UserMessageSink,
) {
    let hint = if supported.is_empty() {
        format!(
            "launch-time registration is not supported for agent '{agent}'; store the \
             credential yourself with `sbx secret set <service> --sandbox <name>`"
        )
    } else {
        format!(
            "supported auth variables for '{agent}': {}",
            supported.join(", ")
        )
    };
    sink.write_message(warning(format!(
        "sbx: env var '{key}' is not a supported sbx auth variable for agent \
         '{agent}' and was withheld ({hint})."
    )));
}

/// Store `value` for `service`, scoped to `sandbox`, with the value and a
/// newline on stdin (contract 5.11). The announcement shows `(value piped
/// via stdin)` and the value is redacted from every published line.
/// UNVERIFIED-V6: that `sbx secret set` reads the value from a pipe.
fn set_secret_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    value: &str,
    sandbox: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    // One canonical representation for transport and redaction. Never send
    // multiline values: the CLI may normalize them in diagnostics.
    let value = value.trim();
    if value.is_empty() || value.contains(['\r', '\n']) {
        return Err(EngineError::Sandbox(
            "SBX provider credentials must be nonempty single-line tokens".into(),
        ));
    }
    let mut stdin = value.as_bytes().to_vec();
    stdin.push(b'\n');
    SbxCommand::new(cli::secret_set_argv(service, sandbox))
        .in_env(snapshot)
        .with_stdin(stdin)
        .announce_suffix("(value piped via stdin)")
        .redact(value.to_string())
        .run_announced(sink)
        .map(|_| ())
}

fn warning(text: String) -> crate::data::message::UserMessage {
    crate::data::message::UserMessage {
        level: crate::data::message::MessageLevel::Warning,
        text,
    }
}

fn info(text: String) -> crate::data::message::UserMessage {
    crate::data::message::UserMessage {
        level: crate::data::message::MessageLevel::Info,
        text,
    }
}

/// Whether a secret for `service` is stored for `sandbox` or globally
/// (contract 5.12). Quiet: a probe publishes nothing on success.
pub(super) fn secret_stored_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    sandbox: &str,
) -> bool {
    [
        cli::secret_ls_sandbox_argv(service, sandbox),
        cli::secret_ls_global_argv(service),
    ]
    .into_iter()
    .any(|args| {
        SbxCommand::new(args)
            .in_env(snapshot)
            .run_quiet()
            .is_ok_and(|out| out.success() && cli::secret_listing_present(&out.stdout))
    })
}

/// Register keychain-resolved credentials. Returns the services registered.
fn register_keychain_credentials(
    snapshot: &crate::data::config::env::EnvSnapshot,
    creds: &[(String, String)],
    sandbox: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<Vec<&'static str>, EngineError> {
    let mut registered = Vec::new();
    for (key, value) in creds {
        // An OAuth token is not an API key: `sbx` has no OAuth secret type
        // for it, so it is never stored. Checked by name before the service
        // mapping, which maps it to "anthropic" for other runtimes.
        if key == "CLAUDE_CODE_OAUTH_TOKEN" {
            continue;
        }
        match service_for_credential(key) {
            Some(service) => {
                set_secret_in(snapshot, service, value, sandbox, sink)?;
                if !registered.contains(&service) {
                    registered.push(service);
                }
            }
            None => {
                sink.write_message(warning(format!(
                    "sbx: credential '{key}' has no known sbx service mapping and was \
                     not registered. Store it yourself with `sbx secret set <service> \
                     --sandbox {sandbox}`."
                )));
            }
        }
    }
    Ok(registered)
}

/// The one-time warning on the first create of an owned-workload sandbox:
/// Docker requires the user's approval before a third-party kit may use a
/// stored credential (DOC CRED). awman never writes that binding.
/// UNVERIFIED-S1: how an unbound binding is reported, so this can become a
/// precise check.
pub(super) fn credential_approval_warning(handle: &str, service: &str, sandbox: &str) -> String {
    format!(
        "Docker Sandboxes requires your approval before the '{handle}' kit may use the \
         '{service}' credential. If the agent reports missing authentication, approve it by \
         running 'sbx run --name {sandbox}' once in a terminal, or add the binding described \
         at https://docs.docker.com/ai/sandboxes/configuration/credentials/"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::{MessageLevel, RecordingMessageSink};

    #[test]
    fn credential_like_names_are_recognised() {
        for key in ["ANTHROPIC_API_KEY", "GH_TOKEN", "MY_SECRET", "DB_PASSWORD"] {
            assert!(is_credential_like(key), "{key}");
        }
        for key in ["PLAIN", "RUST_LOG", "EDITOR"] {
            assert!(!is_credential_like(key), "{key}");
        }
    }

    #[test]
    fn missing_credentials_warn_with_the_sandbox_scoped_command_and_never_claim_auth() {
        let snapshot = crate::data::config::env::EnvSnapshot::empty();
        let mut sink = RecordingMessageSink::new();
        let lookups = CredentialLookups {
            env: &|_| None,
            stored: &|_| false,
        };
        register_launch_credentials(
            &snapshot,
            "claude",
            "awman-1-claude",
            (&[EnvVar("ANTHROPIC_API_KEY".into())], &[]),
            &[],
            &lookups,
            &mut sink,
        )
        .unwrap();
        let text: Vec<String> = sink.all().iter().map(|m| m.text.clone()).collect();
        let joined = text.join("\n");
        assert!(joined.contains("ANTHROPIC_API_KEY is not set"), "{joined}");
        assert!(
            joined.contains("sbx secret set anthropic --sandbox awman-1-claude"),
            "{joined}"
        );
        assert!(!joined.to_lowercase().contains("authenticated"), "{joined}");
        assert!(!joined.contains(" -g"), "{joined}");

        let mut sink = RecordingMessageSink::new();
        let lookups = CredentialLookups {
            env: &|_| None,
            stored: &|service| service == "anthropic",
        };
        register_launch_credentials(
            &snapshot,
            "claude",
            "awman-1-claude",
            (&[], &[]),
            &[],
            &lookups,
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.all().len(), 1);
        assert_eq!(sink.all()[0].level, MessageLevel::Info);
        assert!(sink.all()[0].text.contains("already stored"));
        assert!(sink.all()[0].text.contains("does not check"));
    }

    #[test]
    fn credential_literals_are_withheld_and_owned_kits_get_the_approval_warning() {
        let snapshot = crate::data::config::env::EnvSnapshot::empty();
        let mut sink = RecordingMessageSink::new();
        let lookups = CredentialLookups {
            env: &|_| None,
            stored: &|_| true,
        };
        register_launch_credentials(
            &snapshot,
            "cline",
            "awman-1-cline",
            (
                &[EnvVar("OTHER_TOKEN".into())],
                &[EnvLiteral {
                    key: "OPENAI_API_KEY".into(),
                    value: "sk-literal".into(),
                }],
            ),
            &[],
            &lookups,
            &mut sink,
        )
        .unwrap();
        let joined: String = sink
            .all()
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("'OTHER_TOKEN'"), "{joined}");
        assert!(joined.contains("'OPENAI_API_KEY' was withheld"), "{joined}");
        assert!(!joined.contains("sk-literal"), "{joined}");
        let text = credential_approval_warning("awman-opencode", "anthropic", "awman-1-opencode");
        assert!(text.contains("'awman-opencode' kit"));
        assert!(text.contains("sbx run --name awman-1-opencode"));
    }
}
