//! Credential injection for the Docker Sandbox runtime.
//!
//! Maps awman's credential env-var names to `sbx` service names and registers
//! each value with `sbx secret set <sandbox> <service>`, piping the value via
//! **stdin** so it never appears in argv (process listings) and never lands in
//! the workspace-readable `session.json`.
//!
//! All registration is **sandbox-scoped and happens at agent-launch time** —
//! never `awman ready`, and never globally (`-g`). Scoped secrets take effect
//! immediately (global ones only apply at sandbox creation), awman's keys
//! never leak into sandboxes created outside awman, and `sbx rm` takes the
//! secret scope with it. The launcher guarantees the sandbox exists (created
//! via `sbx create`) before any secret is set.

use crate::data::message::UserMessageSink;
use crate::engine::agent::first_party::first_party;
use crate::engine::container::options::{EnvLiteral, EnvVar};
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::spawn::SbxCommand;

/// Re-export the shared service→credential mapping so the dsbx module (and its
/// tests via `use super::*`) can use it without a long path. The canonical
/// definition lives in [`crate::engine::auth::service_for_credential`].
pub(super) use crate::engine::auth::service_for_credential;

/// Allowlist of provider auth env vars accepted via `env(VAR)` overlays for
/// launch-time auto-auth, read from the first-party table
/// (`AgentDefinition::sandbox_auth_env_vars`, F-32). Any other name (custom
/// agents never reach sbx) has no entry and therefore no allowlist.
///
/// This used to be a second `match agent` here — a per-agent table outside
/// the agent table, and one the F-32 acceptance grep could not see because it
/// matched a `&str` parameter rather than `agent.as_str()`.
pub(super) fn supported_auth_env_vars(agent: &str) -> Vec<String> {
    first_party(agent)
        .map(|d| d.sandbox_auth_env_vars)
        .unwrap_or_default()
}

/// Heuristic: does this env-var name look like it carries a secret? Used to
/// keep credential-class values out of the workspace-readable `session.json`
/// even when they have no explicit `sbx` service mapping.
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

pub(super) fn auto_auth_env_overlays_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
    sandbox: &str,
    overlays: (&[EnvVar], &[EnvLiteral]),
    lookup_env: &dyn Fn(&str) -> Option<String>,
    secret_already_registered: &dyn Fn(&str) -> bool,
    sink: &mut dyn UserMessageSink,
) -> Result<bool, EngineError> {
    let (env_passthrough, env_literal) = overlays;
    let supported = supported_auth_env_vars(agent);
    let mut configured_services: Vec<&'static str> = Vec::new();

    for var in env_passthrough {
        let key = var.0.as_str();
        if supported.iter().any(|s| s == key) {
            let Some(service) = service_for_credential(key) else {
                // Unreachable while the allowlist test below holds; degrade to
                // the unsupported-var warning rather than panicking.
                warn_unsupported_dropped(agent, key, &supported, sink);
                continue;
            };
            if configured_services.contains(&service) {
                continue; // e.g. both GH_TOKEN and GITHUB_TOKEN requested
            }
            match lookup_env(key) {
                Some(value) if !value.is_empty() => {
                    set_secret_in(snapshot, service, &value, sandbox, sink)?;
                    configured_services.push(service);
                }
                _ => {
                    sink.write_message(warning(format!(
                        "sbx: env({key}) was requested but {key} is not set in the \
                         host environment; no '{service}' credential was registered."
                    )));
                }
            }
        } else if is_credential_like(key) {
            warn_unsupported_dropped(agent, key, &supported, sink);
        }
        // Non-credential vars flow to the agent via session.json as before.
    }

    for lit in env_literal {
        if is_credential_like(&lit.key) {
            sink.write_message(warning(format!(
                "sbx: credential-class env literal '{}' was withheld from the \
                 workspace-readable session.json. Pass it as an env({}) overlay to \
                 enable launch-time auto-auth, or register it with `sbx secret set`.",
                lit.key, lit.key
            )));
        }
    }

    if !supported.is_empty() && configured_services.is_empty() {
        // No env() overlay registered a credential this launch. Before warning
        // that auth is unconfigured, probe for a pre-existing sbx secret
        // (sandbox-scoped or global) that already covers the agent's provider —
        // a previous launch, a manual `sbx secret set`, or a global key all
        // count. Deduplicate services so e.g. copilot's GH_TOKEN/GITHUB_TOKEN
        // pair probes `github` once.
        let mut services: Vec<&'static str> = Vec::new();
        for var in &supported {
            if let Some(service) = service_for_credential(var) {
                if !services.contains(&service) {
                    services.push(service);
                }
            }
        }
        let covered: Vec<&str> = services
            .iter()
            .copied()
            .filter(|service| secret_already_registered(service))
            .collect();

        if covered.is_empty() {
            sink.write_message(warning(format!(
                "sbx: no env(...) overlay supplied a supported auth variable for \
                 '{agent}' (supported: {}), and no pre-existing sbx secret for {} \
                 was found (checked `sbx secret ls --service <service>` and `sbx \
                 secret ls -g`). The sbx proxy has no credential awman can use — \
                 register one manually with `sbx secret set {sandbox} <service>` \
                 (sandbox-scoped secrets apply immediately, even while running), \
                 or complete the agent's login flow inside the sandbox. Launching \
                 anyway.",
                supported.join(", "),
                services.join(", ")
            )));
        } else {
            sink.write_message(info(format!(
                "sbx: no env(...) auth overlay was passed for '{agent}', but a \
                 pre-existing sbx secret for {} is already registered (sandbox-scoped \
                 or global) and will authenticate the agent. Launching.",
                covered.join(", ")
            )));
        }
    }

    Ok(!configured_services.is_empty())
}

fn warn_unsupported_dropped(
    agent: &str,
    key: &str,
    supported: &[String],
    sink: &mut dyn UserMessageSink,
) {
    let hint = if supported.is_empty() {
        format!(
            "launch-time auto-auth is not yet supported for agent '{agent}'; register \
             the credential manually with `sbx secret set`"
        )
    } else {
        format!(
            "supported auth variables for '{agent}': {}",
            supported.join(", ")
        )
    };
    sink.write_message(warning(format!(
        "sbx: env var '{key}' is not a supported sbx auth variable for agent \
         '{agent}' and was dropped ({hint})."
    )));
}

/// Register `value` for `service` with `sbx secret set <sandbox> <service>`,
/// piping the value via stdin. Always sandbox-scoped, never global (`-g`):
/// scoped secrets take effect immediately (running or stopped), while global
/// ones only apply at sandbox creation — and scoped registration keeps
/// awman's keys out of sandboxes it does not own.
fn set_secret_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
    value: &str,
    sandbox: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    run_secret_set_in(
        snapshot,
        vec![
            "secret".into(),
            "set".into(),
            sandbox.into(),
            service.into(),
        ],
        value,
        sink,
    )
}

/// One `sbx secret set` invocation with the value piped via stdin.
///
/// The sbx docs are inconsistent about whether headless input needs a
/// `--password-stdin` flag; plain stdin piping is confirmed working against
/// the current sbx, so it is the primary form. When it fails with an error
/// that looks like sbx tried to prompt interactively, retry once with
/// `--password-stdin` appended before giving up.
fn run_secret_set_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    args: Vec<String>,
    value: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    let attempt = SbxCommand::new(args.clone())
        .in_env(snapshot)
        .with_stdin(value.as_bytes().to_vec())
        .announce_suffix("(value piped via stdin)")
        .redact(value.to_string())
        .run_announced(sink);
    match attempt {
        Ok(_) => Ok(()),
        Err(e) if error_suggests_password_stdin(&e) => {
            let mut retry = args;
            retry.push("--password-stdin".to_string());
            SbxCommand::new(retry)
                .in_env(snapshot)
                .with_stdin(value.as_bytes().to_vec())
                .announce_suffix("(value piped via stdin)")
                .redact(value.to_string())
                .run_announced(sink)
                .map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// Does this `sbx secret set` failure look like sbx expected interactive
/// input (and so may want `--password-stdin`)? Matched against the wrapped
/// error text, which embeds sbx's stderr. "stdin" itself is not a needle —
/// the announcement suffix "(value piped via stdin)" is part of the error
/// string and would always match.
fn error_suggests_password_stdin(e: &EngineError) -> bool {
    let msg = e.to_string().to_lowercase();
    ["tty", "terminal", "interactive", "password"]
        .iter()
        .any(|needle| msg.contains(needle))
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

pub(super) fn secret_registered_for_service_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    service: &str,
) -> bool {
    let queries = [
        vec![
            "secret".to_string(),
            "ls".to_string(),
            "--service".to_string(),
            service.to_string(),
        ],
        vec!["secret".to_string(), "ls".to_string(), "-g".to_string()],
    ];
    for args in queries {
        if let Ok(out) = SbxCommand::new(args).in_env(snapshot).run_quiet() {
            if out.success() && listing_mentions_service(&out.stdout, service) {
                return true;
            }
        }
    }
    false
}

/// Does a `sbx secret ls` listing report a secret for `service`? Treats an
/// empty listing (or the JSON empty array `[]`) as "no secret", and otherwise
/// looks for the service name in the output. Conservative by design: an
/// ambiguous listing should not suppress the manual-auth warning.
fn listing_mentions_service(stdout: &str, service: &str) -> bool {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return false;
    }
    trimmed.contains(service)
}

pub(super) fn inject_credentials_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    creds: &[(String, String)],
    sandbox: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    for (key, value) in creds {
        // Sbx has no OAuth-token support yet; skip unconditionally.  See the
        // doc comment above.  Key-name check takes precedence over the service
        // mapping so this remains correct even though service_for_credential
        // now maps CLAUDE_CODE_OAUTH_TOKEN → "anthropic" for dedup use by
        // other runtimes.
        if key == "CLAUDE_CODE_OAUTH_TOKEN" {
            continue;
        }
        match service_for_credential(key) {
            Some(service) => {
                set_secret_in(snapshot, service, value, sandbox, sink)?;
            }
            None => {
                sink.write_message(crate::data::message::UserMessage {
                    level: crate::data::message::MessageLevel::Warning,
                    text: format!(
                        "sbx: credential '{key}' has no known sbx service mapping and was \
                         not injected. Declare it under `credentials.sources` in the agent \
                         kit, or register it manually with `sbx secret set`."
                    ),
                });
            }
        }
    }
    Ok(())
}
