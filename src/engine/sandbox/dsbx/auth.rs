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
use crate::engine::container::options::{EnvLiteral, EnvVar};
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::spawn::SbxCommand;

/// Re-export the shared service→credential mapping so the dsbx module (and its
/// tests via `use super::*`) can use it without a long path. The canonical
/// definition lives in [`crate::engine::auth::service_for_credential`].
pub(super) use crate::engine::auth::service_for_credential;

/// Allowlist of provider auth env vars accepted via `env(VAR)` overlays for
/// launch-time auto-auth, per agent. Only mixin-kit agents participate —
/// agent-kit agents (antigravity, crush, maki, cline) are intentionally left
/// out for now, so they return an empty list.
///
/// Each var must satisfy two constraints, both verified against the Docker
/// Sandboxes credentials docs: the var maps to an sbx well-known service
/// ([`service_for_credential`]), and the agent's base kit actually routes that
/// service through the host proxy (the kit template's `network.allowedDomains`
/// / `environment.proxyManaged`).
pub(super) fn supported_auth_env_vars(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &["ANTHROPIC_API_KEY"],
        "codex" => &["OPENAI_API_KEY"],
        "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "copilot" => &["GH_TOKEN", "GITHUB_TOKEN"],
        "opencode" => &["ANTHROPIC_API_KEY"],
        _ => &[],
    }
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

/// Launch-time auto-auth driven by `env(VAR)` overlays.
///
/// For each passthrough var on the agent's [`supported_auth_env_vars`]
/// allowlist, the host value is registered with `sbx secret set <sandbox>
/// <service>` (piped via stdin, redacted) so the sbx host proxy is
/// authenticated by the time the agent starts. Runs at agent-launch time —
/// not `awman ready` — so rotated keys take effect without re-running ready.
/// `sandbox` must already exist (the launcher creates it first).
///
/// Everything else is a warning, never a silent drop and never a launch
/// abort:
/// - a supported var whose host value is unset,
/// - a credential-class var outside the allowlist (dropped — it is already
///   excluded from the workspace-readable `session.json`),
/// - a credential-class `env_literal` (withheld; point at `env(VAR)`),
/// - an allowlisted agent launching with no supported auth overlay at all
///   (auth must be set up manually or via in-sandbox login).
///
/// `lookup_env` abstracts `std::env::var` so tests don't mutate process-global
/// state. A failed `sbx secret set` subprocess is launch-blocking (WI 0090).
///
/// `secret_already_registered` abstracts the read-only `sbx secret ls` probe
/// (production: [`secret_registered_for_service`]) so tests stay hermetic. It
/// is consulted only on the no-overlay path: before warning that auth is
/// unconfigured, awman checks whether a pre-existing sbx secret (sandbox-scoped
/// or global) already authenticates the agent's provider, and downgrades the
/// warning to an informational note when one is found.
///
/// Returns `true` when at least one supported auth var was read from the host
/// and registered with `sbx secret set` — i.e. the sbx proxy now has a
/// credential awman put there.
pub(super) fn auto_auth_env_overlays(
    agent: &str,
    sandbox: &str,
    env_passthrough: &[EnvVar],
    env_literal: &[EnvLiteral],
    lookup_env: &dyn Fn(&str) -> Option<String>,
    secret_already_registered: &dyn Fn(&str) -> bool,
    sink: &mut dyn UserMessageSink,
) -> Result<bool, EngineError> {
    let supported = supported_auth_env_vars(agent);
    let mut configured_services: Vec<&'static str> = Vec::new();

    for var in env_passthrough {
        let key = var.0.as_str();
        if supported.contains(&key) {
            let Some(service) = service_for_credential(key) else {
                // Unreachable while the allowlist test below holds; degrade to
                // the unsupported-var warning rather than panicking.
                warn_unsupported_dropped(agent, key, supported, sink);
                continue;
            };
            if configured_services.contains(&service) {
                continue; // e.g. both GH_TOKEN and GITHUB_TOKEN requested
            }
            match lookup_env(key) {
                Some(value) if !value.is_empty() => {
                    set_secret(service, &value, sandbox, sink)?;
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
            warn_unsupported_dropped(agent, key, supported, sink);
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
        for var in supported {
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
    supported: &[&str],
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
fn set_secret(
    service: &str,
    value: &str,
    sandbox: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    run_secret_set(
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
/// The sbx docs are inconsistent about whether non-interactive input needs a
/// `--password-stdin` flag; plain stdin piping is confirmed working against
/// the current sbx, so it is the primary form. When it fails with an error
/// that looks like sbx tried to prompt interactively, retry once with
/// `--password-stdin` appended before giving up.
fn run_secret_set(
    args: Vec<String>,
    value: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    let attempt = SbxCommand::new(args.clone())
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

/// Best-effort, read-only probe: does a pre-existing sbx secret already cover
/// `service` (e.g. `anthropic`, `openai`, `github`)?
///
/// Checks both a service-scoped listing (`sbx secret ls --service <service>`)
/// and the global store (`sbx secret ls -g`). The query runs quietly — these
/// are decision probes, not actions, so they are deliberately not announced on
/// the sink to avoid two extra "Running: …" lines on every no-overlay launch.
///
/// Fail-safe: if `sbx` is absent or either listing exits non-zero, the probe
/// returns `false`, so the manual-auth warning still fires rather than being
/// suppressed on a faulty assumption. The `<service>` value passed to
/// `--service` is the Docker Sandboxes well-known service name (per
/// [`service_for_credential`]), e.g. `anthropic` for the `claude` agent — never
/// the agent name.
pub(super) fn secret_registered_for_service(service: &str) -> bool {
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
        if let Ok(out) = SbxCommand::new(args).run_quiet() {
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

/// Register awman-resolved credentials (keychain etc.) with `sbx secret set`.
///
/// Mapped credentials are piped to `sbx secret set <sandbox> <service>` via
/// stdin — sandbox-scoped at launch time, like all sbx secret registration.
/// Unmapped credentials are not silent failures: a warning names the variable
/// and suggests the kit-level `environment.proxyManaged` route. The value is
/// never written anywhere awman controls on the host.
///
/// `CLAUDE_CODE_OAUTH_TOKEN` is the one silent exception: the keychain
/// resolver always surfaces it when the user is logged in to Claude Code, but
/// sbx has no OAuth-token support yet (docker/sbx-releases#11), so it can
/// never be injected. The service mapping in [`service_for_credential`] maps
/// it to "anthropic" for dedup purposes on other runtimes — but the sbx
/// driver skips it unconditionally (key-name check before the service lookup).
/// Auth still works without it — via an `ANTHROPIC_API_KEY` env() overlay
/// ([`auto_auth_env_overlays`]) or `/login` inside the sandbox — and the
/// missing-auth case already gets its own warning from
/// [`auto_auth_env_overlays`], so warning here on every launch is pure noise.
pub(super) fn inject_credentials(
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
                set_secret(service, value, sandbox, sink)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::MessageLevel;

    #[test]
    fn known_services_map() {
        assert_eq!(
            service_for_credential("ANTHROPIC_API_KEY"),
            Some("anthropic")
        );
        // OAuth token maps to anthropic for injection-time dedup on container
        // runtimes.  The sbx driver skips it via a key-name guard before
        // reaching this table (sbx has no OAuth support yet).
        assert_eq!(
            service_for_credential("CLAUDE_CODE_OAUTH_TOKEN"),
            Some("anthropic")
        );
        assert_eq!(service_for_credential("OPENAI_API_KEY"), Some("openai"));
        assert_eq!(service_for_credential("GH_TOKEN"), Some("github"));
        assert_eq!(service_for_credential("GITHUB_TOKEN"), Some("github"));
        assert_eq!(service_for_credential("GEMINI_API_KEY"), Some("google"));
        assert_eq!(service_for_credential("AWS_ACCESS_KEY_ID"), Some("aws"));
        assert_eq!(service_for_credential("AWS_SECRET_ACCESS_KEY"), Some("aws"));
        assert_eq!(service_for_credential("GROQ_API_KEY"), Some("groq"));
        assert_eq!(service_for_credential("MISTRAL_API_KEY"), Some("mistral"));
    }

    #[test]
    fn unknown_service_is_none() {
        assert_eq!(service_for_credential("SOME_RANDOM_VAR"), None);
    }

    #[test]
    fn credential_like_heuristic() {
        assert!(is_credential_like("ANTHROPIC_API_KEY"));
        assert!(is_credential_like("MY_SERVICE_TOKEN"));
        assert!(is_credential_like("DB_PASSWORD"));
        assert!(!is_credential_like("LOG_LEVEL"));
        assert!(!is_credential_like("WORKDIR"));
    }

    // ─── inject_credentials behaviour ─────────────────────────────────────

    #[test]
    fn unmapped_credential_produces_warning_not_error() {
        let mut sink = VecSink::default();
        let creds = vec![("SOME_UNKNOWN_VAR".to_string(), "value".to_string())];
        // Must not return Err — unmapped credentials are soft-skipped.
        let result = inject_credentials(&creds, "awman-h-claude", &mut sink);
        assert!(
            result.is_ok(),
            "unmapped credential must not produce an error"
        );
        assert!(
            sink.messages.iter().any(|m| {
                m.level == MessageLevel::Warning
                    && m.text.contains("SOME_UNKNOWN_VAR")
                    && m.text.contains("no known sbx service mapping")
            }),
            "must produce a Warning naming the variable; messages: {:?}",
            sink.messages
        );
    }

    #[test]
    fn claude_oauth_token_is_silently_skipped() {
        // The keychain resolver surfaces CLAUDE_CODE_OAUTH_TOKEN on every
        // launch when the user is logged in to Claude Code, but sbx cannot use
        // it — it is skipped without an error AND without a warning (the
        // missing-auth case is covered by auto_auth_env_overlays).
        let mut sink = VecSink::default();
        let creds = vec![(
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            "sk-ant-oat-secret".to_string(),
        )];
        let result = inject_credentials(&creds, "awman-h-claude", &mut sink);
        assert!(
            result.is_ok(),
            "unsupported OAuth token must not produce an error"
        );
        assert!(
            sink.messages.is_empty(),
            "OAuth token must be skipped without any message; messages: {:?}",
            sink.messages
        );
    }

    #[test]
    fn mapped_credential_announcement_uses_stdin_suffix_not_value() {
        // run_announced() writes the announcement BEFORE calling run_quiet(),
        // so even when sbx is absent we can inspect the sink for the announcement.
        let mut sink = VecSink::default();
        let creds = vec![(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-supersecret".to_string(),
        )];
        // Ignore Ok/Err — sbx may not be installed.
        let _ = inject_credentials(&creds, "awman-h-claude", &mut sink);
        let announcement = sink
            .messages
            .iter()
            .find(|m| m.level == MessageLevel::Info && m.text.contains("sbx secret set"));
        let msg = announcement.expect("must write an Info announcement for a mapped credential");
        assert!(
            msg.text.contains("(value piped via stdin)"),
            "announcement must describe stdin delivery; got: {:?}",
            msg.text
        );
        assert!(
            !msg.text.contains("sk-supersecret"),
            "secret value must not appear in the announcement: {:?}",
            msg.text
        );
    }

    #[test]
    fn mapped_credential_argv_does_not_contain_service_value() {
        // Structural guarantee: service_for_credential returns the service name
        // and inject_credentials passes it as an argument to `sbx secret set
        // <sandbox> <service>` — the credential VALUE is never in the argv.
        let service = service_for_credential("OPENAI_API_KEY").unwrap();
        assert_eq!(service, "openai");
        // The argv built is ["secret", "set", "<sandbox>", "openai"]. The value
        // "sk-xyz" would only appear if accidentally added to args. We verify
        // this indirectly via the display_line shape:
        let display = format!("sbx secret set awman-h-codex {service} (value piped via stdin)");
        assert!(
            !display.contains("sk-xyz"),
            "service name must not be a value"
        );
        assert!(
            display.contains("openai"),
            "service name must appear in announcement"
        );
    }

    #[test]
    fn all_credential_table_entries_have_expected_service_names() {
        let table: &[(&str, &str)] = &[
            ("ANTHROPIC_API_KEY", "anthropic"),
            // OAuth token: maps to anthropic for dedup; sbx skips it via
            // key-name guard before reaching this table.
            ("CLAUDE_CODE_OAUTH_TOKEN", "anthropic"),
            ("OPENAI_API_KEY", "openai"),
            ("GH_TOKEN", "github"),
            ("GITHUB_TOKEN", "github"),
            ("GEMINI_API_KEY", "google"),
            ("AWS_ACCESS_KEY_ID", "aws"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("GROQ_API_KEY", "groq"),
            ("MISTRAL_API_KEY", "mistral"),
        ];
        for (key, expected_service) in table {
            assert_eq!(
                service_for_credential(key),
                Some(*expected_service),
                "key {key} should map to {expected_service}"
            );
        }
    }

    #[test]
    fn inject_empty_creds_is_noop_ok() {
        let mut sink = VecSink::default();
        let result = inject_credentials(&[], "awman-h-claude", &mut sink);
        assert!(result.is_ok());
        assert!(sink.messages.is_empty());
    }

    // ─── Launch-time env() overlay auto-auth ──────────────────────────────

    use crate::engine::container::options::{EnvLiteral, EnvVar};

    fn passthrough(keys: &[&str]) -> Vec<EnvVar> {
        keys.iter().map(|k| EnvVar((*k).to_string())).collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Default secret probe for tests: no pre-existing sbx secret, so the
    /// no-overlay path takes the manual-auth warning branch.
    fn no_secret(_: &str) -> bool {
        false
    }

    #[test]
    fn allowlist_covers_mixin_agents_only() {
        // Verified against docs.docker.com/ai/sandboxes/security/credentials/:
        // service ↔ env var pairs per built-in agent.
        assert_eq!(supported_auth_env_vars("claude"), &["ANTHROPIC_API_KEY"]);
        assert_eq!(supported_auth_env_vars("codex"), &["OPENAI_API_KEY"]);
        assert_eq!(
            supported_auth_env_vars("gemini"),
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        );
        assert_eq!(
            supported_auth_env_vars("copilot"),
            &["GH_TOKEN", "GITHUB_TOKEN"]
        );
        assert_eq!(supported_auth_env_vars("opencode"), &["ANTHROPIC_API_KEY"]);
        // Agent-kit agents are out of scope for now.
        for agent in ["antigravity", "crush", "maki", "cline", "unknown"] {
            assert!(
                supported_auth_env_vars(agent).is_empty(),
                "agent-kit agent {agent} must have no auto-auth allowlist"
            );
        }
    }

    #[test]
    fn every_allowlisted_var_maps_to_an_sbx_service() {
        // Guards the non-panicking fallback in auto_auth_env_overlays: a
        // supported var without a service mapping is a table bug.
        for agent in ["claude", "codex", "gemini", "copilot", "opencode"] {
            for var in supported_auth_env_vars(agent) {
                assert!(
                    service_for_credential(var).is_some(),
                    "allowlisted var {var} (agent {agent}) must map to an sbx service"
                );
            }
        }
    }

    #[test]
    fn missing_host_value_warns_and_then_warns_manual_auth() {
        let mut sink = VecSink::default();
        let result = auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &passthrough(&["ANTHROPIC_API_KEY"]),
            &[],
            &no_env,
            &no_secret,
            &mut sink,
        );
        assert!(result.is_ok(), "missing host value must not block launch");
        assert!(
            sink.messages.iter().any(|m| {
                m.level == MessageLevel::Warning
                    && m.text.contains("ANTHROPIC_API_KEY is not set in the host")
            }),
            "must warn the requested var is unset; messages: {:?}",
            sink.messages
        );
        assert!(
            sink.messages.iter().any(|m| {
                m.level == MessageLevel::Warning && m.text.contains("Launching anyway")
            }),
            "no credential was registered, so the manual-auth warning must follow"
        );
    }

    #[test]
    fn unsupported_credential_var_is_dropped_with_warning() {
        let mut sink = VecSink::default();
        // OPENAI_API_KEY is a valid sbx var but not for claude.
        let result = auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &passthrough(&["OPENAI_API_KEY"]),
            &[],
            &|_| Some("sk-value".into()),
            &no_secret,
            &mut sink,
        );
        assert!(result.is_ok());
        let dropped = sink
            .messages
            .iter()
            .find(|m| m.text.contains("OPENAI_API_KEY") && m.text.contains("dropped"))
            .expect("unsupported var must produce a dropped warning");
        assert_eq!(dropped.level, MessageLevel::Warning);
        assert!(
            dropped.text.contains("ANTHROPIC_API_KEY"),
            "warning must list the agent's supported vars: {}",
            dropped.text
        );
        assert!(
            !sink.messages.iter().any(|m| m.text.contains("sk-value")),
            "the value must never reach the sink"
        );
    }

    #[test]
    fn non_credential_var_passes_silently() {
        let mut sink = VecSink::default();
        auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &passthrough(&["LOG_LEVEL"]),
            &[],
            &|_| Some("debug".into()),
            &no_secret,
            &mut sink,
        )
        .unwrap();
        assert!(
            !sink.messages.iter().any(|m| m.text.contains("LOG_LEVEL")),
            "non-credential vars are session.json passthrough, not auth: {:?}",
            sink.messages
        );
    }

    #[test]
    fn agent_kit_agent_gets_drop_warning_but_no_manual_auth_warning() {
        let mut sink = VecSink::default();
        auto_auth_env_overlays(
            "crush",
            "awman-h-crush",
            &passthrough(&["ANTHROPIC_API_KEY"]),
            &[],
            &|_| Some("sk-value".into()),
            &no_secret,
            &mut sink,
        )
        .unwrap();
        assert!(
            sink.messages
                .iter()
                .any(|m| { m.text.contains("not yet supported for agent 'crush'") }),
            "agent-kit agents must explain auto-auth is out of scope: {:?}",
            sink.messages
        );
        assert!(
            !sink
                .messages
                .iter()
                .any(|m| m.text.contains("Launching anyway")),
            "the no-auth-overlay warning is mixin-only"
        );
    }

    #[test]
    fn credential_class_env_literal_is_withheld_with_overlay_hint() {
        let mut sink = VecSink::default();
        auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &[],
            &[EnvLiteral {
                key: "MY_DB_PASSWORD".into(),
                value: "hunter2".into(),
            }],
            &no_env,
            &no_secret,
            &mut sink,
        )
        .unwrap();
        let withheld = sink
            .messages
            .iter()
            .find(|m| m.text.contains("MY_DB_PASSWORD") && m.text.contains("withheld"))
            .expect("credential-class literal must be warned about");
        assert!(
            withheld.text.contains("env(MY_DB_PASSWORD)"),
            "warning must point at the env() overlay route: {}",
            withheld.text
        );
        assert!(
            !sink.messages.iter().any(|m| m.text.contains("hunter2")),
            "literal values must never reach the sink"
        );
    }

    // ─── Pre-existing secret probe on the no-overlay path ─────────────────

    #[test]
    fn pre_existing_secret_suppresses_manual_auth_warning() {
        // No env() overlay, but the probe reports a registered secret for the
        // agent's provider — the manual-auth warning is replaced by an Info note.
        let mut sink = VecSink::default();
        let probed: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let result = auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &[],
            &[],
            &no_env,
            &|service| {
                probed.borrow_mut().push(service.to_string());
                service == "anthropic"
            },
            &mut sink,
        );
        assert!(result.is_ok());
        assert!(
            probed.borrow().iter().any(|s| s == "anthropic"),
            "the probe must be consulted for the agent's sbx service, not its name; \
             probed: {:?}",
            probed.borrow()
        );
        assert!(
            !sink
                .messages
                .iter()
                .any(|m| m.text.contains("Launching anyway")),
            "a pre-existing secret must suppress the manual-auth warning: {:?}",
            sink.messages
        );
        assert!(
            sink.messages.iter().any(|m| {
                m.level == MessageLevel::Info
                    && m.text.contains("pre-existing sbx secret for anthropic")
            }),
            "must note the pre-existing credential at Info level: {:?}",
            sink.messages
        );
    }

    #[test]
    fn no_pre_existing_secret_still_warns_manual_auth() {
        // The probe reports nothing for the provider, so the manual-auth warning
        // fires and names the service that was checked.
        let mut sink = VecSink::default();
        auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &[],
            &[],
            &no_env,
            &no_secret,
            &mut sink,
        )
        .unwrap();
        let warn = sink
            .messages
            .iter()
            .find(|m| m.text.contains("Launching anyway"))
            .expect("missing auth with no pre-existing secret must warn");
        assert_eq!(warn.level, MessageLevel::Warning);
        assert!(
            warn.text
                .contains("no pre-existing sbx secret for anthropic"),
            "warning must name the service it probed: {}",
            warn.text
        );
    }

    #[test]
    fn listing_mentions_service_treats_empty_and_array_as_absent() {
        assert!(!listing_mentions_service("", "anthropic"));
        assert!(!listing_mentions_service("   \n", "anthropic"));
        assert!(!listing_mentions_service("[]", "anthropic"));
        assert!(listing_mentions_service("anthropic  set", "anthropic"));
        assert!(!listing_mentions_service("openai  set", "anthropic"));
    }

    #[cfg(unix)]
    mod with_subprocess {
        use super::*;
        use crate::engine::sandbox::dsbx::test_support::with_fake_sbx;

        #[test]
        fn supported_var_is_registered_sandbox_scoped_never_global() {
            with_fake_sbx("#!/bin/sh\ncat > /dev/null\n", || {
                let mut sink = VecSink::default();
                let registered = auto_auth_env_overlays(
                    "claude",
                    "awman-h-claude",
                    &passthrough(&["ANTHROPIC_API_KEY"]),
                    &[],
                    &|_| Some("sk-ant-secret".into()),
                    &no_secret,
                    &mut sink,
                )
                .unwrap();
                assert!(
                    registered,
                    "a successful secret set must report overlay auth as registered"
                );
                assert!(
                    sink.messages.iter().any(|m| {
                        m.text.contains(
                            "sbx secret set awman-h-claude anthropic (value piped via stdin)",
                        )
                    }),
                    "must announce the sandbox-scoped secret set; messages: {:?}",
                    sink.messages
                );
                assert!(
                    !sink.messages.iter().any(|m| m.text.contains(" -g ")),
                    "secret registration must never be global; messages: {:?}",
                    sink.messages
                );
                assert!(
                    !sink
                        .messages
                        .iter()
                        .any(|m| m.text.contains("sk-ant-secret")),
                    "value must be redacted everywhere"
                );
                assert!(
                    !sink
                        .messages
                        .iter()
                        .any(|m| m.text.contains("Launching anyway")),
                    "a registered credential suppresses the manual-auth warning"
                );
            });
        }

        #[test]
        fn duplicate_vars_for_one_service_register_once() {
            with_fake_sbx("#!/bin/sh\ncat > /dev/null\n", || {
                let mut sink = VecSink::default();
                auto_auth_env_overlays(
                    "copilot",
                    "awman-h-copilot",
                    &passthrough(&["GH_TOKEN", "GITHUB_TOKEN"]),
                    &[],
                    &|_| Some("ghp-secret".into()),
                    &no_secret,
                    &mut sink,
                )
                .unwrap();
                let github_sets = sink
                    .messages
                    .iter()
                    .filter(|m| {
                        m.text
                            .contains("Running: sbx secret set awman-h-copilot github")
                    })
                    .count();
                assert_eq!(
                    github_sets, 1,
                    "one registration per service: {:?}",
                    sink.messages
                );
            });
        }

        #[test]
        fn interactive_prompt_failure_retries_with_password_stdin() {
            // A fake sbx that rejects plain stdin (as some doc versions imply)
            // and only succeeds when --password-stdin is passed.
            with_fake_sbx(
                "#!/bin/sh\n\
                 case \"$@\" in\n\
                   *--password-stdin*) cat > /dev/null; exit 0;;\n\
                   *) echo 'ERROR: cannot prompt for password in non-interactive terminal' >&2; exit 1;;\n\
                 esac\n",
                || {
                    let mut sink = VecSink::default();
                    let result = auto_auth_env_overlays(
                        "claude",
                        "awman-h-claude",
                        &passthrough(&["ANTHROPIC_API_KEY"]),
                        &[],
                        &|_| Some("sk-ant-secret".into()),
                        &no_secret,
                        &mut sink,
                    );
                    assert!(result.is_ok(), "retry with --password-stdin must succeed: {result:?}");
                    assert!(
                        sink.messages.iter().any(|m| {
                            m.text.contains(
                                "sbx secret set awman-h-claude anthropic --password-stdin",
                            )
                        }),
                        "retry announcement must show the flag; messages: {:?}",
                        sink.messages
                    );
                },
            );
        }

        #[test]
        fn unrelated_secret_set_failure_blocks_launch() {
            with_fake_sbx(
                "#!/bin/sh\necho 'ERROR: not logged in; run sbx login' >&2\nexit 1\n",
                || {
                    let mut sink = VecSink::default();
                    let result = auto_auth_env_overlays(
                        "claude",
                        "awman-h-claude",
                        &passthrough(&["ANTHROPIC_API_KEY"]),
                        &[],
                        &|_| Some("sk-ant-secret".into()),
                        &no_secret,
                        &mut sink,
                    );
                    match result {
                        Err(EngineError::Sandbox(msg)) => {
                            assert!(
                                msg.contains("not logged in"),
                                "error must carry sbx's diagnostics: {msg}"
                            );
                            assert!(
                                !msg.contains("sk-ant-secret"),
                                "error must not leak the value: {msg}"
                            );
                        }
                        other => panic!("secret set failure must block launch, got {other:?}"),
                    }
                },
            );
        }

        // ─── secret_registered_for_service probe ──────────────────────────

        #[test]
        fn probe_finds_service_in_scoped_listing() {
            // A fake sbx whose `secret ls --service anthropic` reports the
            // service; `secret ls -g` is never needed.
            with_fake_sbx(
                "#!/bin/sh\n\
                 case \"$*\" in\n\
                   *'--service anthropic'*) echo 'anthropic  set';;\n\
                   *) echo '[]';;\n\
                 esac\n",
                || {
                    assert!(
                        secret_registered_for_service("anthropic"),
                        "a scoped listing naming the service must count as registered"
                    );
                },
            );
        }

        #[test]
        fn probe_falls_back_to_global_listing() {
            // Service-scoped listing is empty, but the global store has it.
            with_fake_sbx(
                "#!/bin/sh\n\
                 case \"$*\" in\n\
                   *-g*) echo 'anthropic  set';;\n\
                   *) echo '';;\n\
                 esac\n",
                || {
                    assert!(
                        secret_registered_for_service("anthropic"),
                        "a global listing naming the service must count as registered"
                    );
                },
            );
        }

        #[test]
        fn probe_returns_false_when_no_listing_mentions_service() {
            with_fake_sbx("#!/bin/sh\necho '[]'\n", || {
                assert!(
                    !secret_registered_for_service("anthropic"),
                    "empty listings must not be treated as a registered secret"
                );
            });
        }

        #[test]
        fn probe_returns_false_on_listing_failure() {
            with_fake_sbx("#!/bin/sh\necho 'ERROR: boom' >&2\nexit 1\n", || {
                assert!(
                    !secret_registered_for_service("anthropic"),
                    "a failing probe must be fail-safe (warning still fires)"
                );
            });
        }
    }

    // ─── VecSink helper ───────────────────────────────────────────────────

    #[derive(Default)]
    struct VecSink {
        messages: Vec<crate::data::message::UserMessage>,
    }
    impl crate::data::message::UserMessageSink for VecSink {
        fn write_message(&mut self, msg: crate::data::message::UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {}
    }
}
