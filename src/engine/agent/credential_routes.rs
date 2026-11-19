//! Definition-selected credential routes. Discovery never reads credentials or launches.
use super::{definition::AgentOrigin, AgentDefinition};
use crate::data::credential_update::*;

fn route(
    kind: CredentialKind,
    delivery: CredentialDelivery,
    backend: Option<&str>,
    owner: RefreshOwner,
) -> CredentialUpdateRoute {
    CredentialUpdateRoute {
        credential_kind: kind,
        delivery,
        model_provider: backend.map(str::to_owned),
        live_update: if delivery == CredentialDelivery::Env {
            Some(false)
        } else {
            None
        },
        restart_supported: None,
        refresh_owner: owner,
        update_boundary: None,
        max_apply_delay: None,
        restart_scope: None,
        conversation_continuity: None,
        reason: Some("runtime_evidence_required".into()),
    }
}
/// Candidate routes are distinct by backend/source/delivery. Custom adapters
/// inherit no first-party credential source privilege.
pub fn declarations(definition: &AgentDefinition) -> CredentialUpdateCapabilities {
    use crate::data::automation::AutomationAdapterKind as A;
    use CredentialDelivery::*;
    use CredentialKind::*;
    use RefreshOwner::{Awman, Caller, Provider};
    let routes = match definition.automation_adapter {
        Some(A::Claude) => vec![
            route(ApiKey, Env, Some("anthropic"), Caller),
            route(OauthAccessToken, Env, Some("anthropic"), Caller),
            route(OauthAccessToken, File, Some("anthropic"), Awman),
            route(ApiKey, Helper, Some("anthropic"), Caller),
            route(ProviderManaged, NativeStore, Some("anthropic"), Provider),
            route(CredentialKind::Unknown, Env, Some("bedrock"), Caller),
            route(CredentialKind::Unknown, Env, Some("vertex"), Caller),
        ],
        Some(A::Codex) => vec![
            route(ApiKey, Env, Some("openai"), Caller),
            route(ApiKey, Helper, None, Caller),
            route(OauthAccessToken, File, Some("chatgpt"), Caller),
            route(ProviderManaged, NativeStore, Some("chatgpt"), Provider),
        ],
        Some(A::Opencode) => vec![
            route(ApiKey, Env, None, Caller),
            route(ApiKey, File, None, Caller),
            route(ProviderManaged, NativeStore, None, Provider),
        ],
        Some(A::OpencodeV2) => vec![
            route(ApiKey, Env, None, Caller),
            route(ApiKey, NativeStore, None, Caller),
            route(ProviderManaged, NativeStore, None, Provider),
        ],
        Some(A::Cursor) => vec![
            route(ApiKey, Env, Some("cursor"), Caller),
            route(ProviderManaged, NativeStore, Some("cursor"), Provider),
        ],
        Some(A::Copilot) => vec![
            route(GithubToken, Env, Some("github"), Caller),
            route(ApiKey, Env, None, Caller),
            route(ProviderManaged, NativeStore, Some("github"), Provider),
        ],
        Some(A::Cline) => vec![
            route(ApiKey, Env, None, Caller),
            route(ApiKey, File, None, Caller),
            route(ProviderManaged, NativeStore, None, Provider),
        ],
        Some(A::Antigravity) => vec![
            route(OauthAccessToken, File, Some("consumer"), Caller),
            route(ApiKey, Env, Some("gemini"), Caller),
            route(ProviderManaged, NativeStore, None, Provider),
        ],
        _ => vec![route(
            CredentialKind::Unknown,
            CredentialDelivery::Unknown,
            None,
            RefreshOwner::None,
        )],
    };
    let routes = if definition.origin != AgentOrigin::FirstParty {
        routes
            .into_iter()
            .filter(|r| matches!(r.delivery, Env | Helper) && r.refresh_owner == Caller)
            .collect()
    } else {
        routes
    };
    CredentialUpdateCapabilities { routes }
}
/// Resolve the effective method from actual runtime facts. Uncertain
/// consumption always forces an established owned restart or reconciliation.
pub fn method(route: &CredentialUpdateRoute, certainty: GenerationState) -> CredentialUpdateMethod {
    route.method(certainty)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_adapters_offer_mechanics_without_first_party_source_privileges() {
        let mut custom = crate::engine::agent::first_party::first_party("claude").unwrap();
        custom.origin = AgentOrigin::Custom {
            scope: super::super::definition::DefinitionScope::Repo,
            path: "definition.toml".into(),
            sha256: "a".repeat(64),
        };
        let routes = declarations(&custom).routes;
        assert!(routes
            .iter()
            .any(|r| r.delivery == CredentialDelivery::Helper));
        assert!(routes
            .iter()
            .all(|r| r.refresh_owner == RefreshOwner::Caller));
        assert!(routes.iter().all(|r| matches!(
            r.delivery,
            CredentialDelivery::Env | CredentialDelivery::Helper
        )));
        custom.automation_adapter = None;
        assert!(declarations(&custom).routes.is_empty());
    }
    #[test]
    fn helpers_require_explicit_grants_exclusive_sources_and_bounded_caches() {
        assert!(claude_helper_override("{}", true, None).unwrap().is_none());
        let settings = r#"{"apiKeyHelper":"/awman/credentials/helper"}"#;
        assert!(claude_helper_override(settings, false, None).is_err());
        assert_eq!(
            claude_helper_override(settings, true, None)
                .unwrap()
                .unwrap()
                .max_apply_delay,
            60_000
        );
        assert!(claude_helper_override(settings, true, Some(300_000)).is_err());
        let config="[model_providers.private.auth]\ncommand='/awman/credentials/helper'\nrefresh_interval_ms=1000";
        assert!(codex_helper_override(config, "other", true)
            .unwrap()
            .is_none());
        assert!(codex_helper_override(config, "private", false).is_err());
        assert_eq!(
            codex_helper_override(config, "private", true)
                .unwrap()
                .unwrap()
                .timeout_ms,
            5000
        );
        let masked="[model_providers.private]\nenv_key='OPENAI_API_KEY'\n[model_providers.private.auth]\ncommand='/awman/credentials/helper'";
        assert!(codex_helper_override(masked, "private", true).is_err());
    }
    #[test]
    fn every_adapter_has_source_routes_and_unknown_native_support_is_not_false() {
        for name in crate::engine::agent::first_party::FIRST_PARTY_AGENTS {
            let definition = crate::engine::agent::first_party::first_party(name).unwrap();
            let routes = declarations(&definition).routes;
            assert!(!routes.is_empty());
            assert!(routes
                .iter()
                .any(|r| r.delivery == CredentialDelivery::Env && r.live_update == Some(false)));
            assert!(routes
                .iter()
                .filter(|r| r.delivery != CredentialDelivery::Env)
                .all(|r| r.live_update.is_none()));
            for mut route in routes {
                route.live_update = Some(true);
                route.restart_supported = Some(true);
                assert_eq!(
                    method(&route, GenerationState::Confirmed),
                    CredentialUpdateMethod::Live
                );
                assert_eq!(
                    method(&route, GenerationState::Unknown),
                    CredentialUpdateMethod::Restart
                );
            }
        }
    }
}

/// Timing evidence for an explicitly selected guest helper. Native schema and
/// exclusive auth resolution still need a runtime probe before live=true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperPolicy {
    pub refresh_interval_ms: u64,
    pub timeout_ms: u64,
    pub max_apply_delay: u64,
}
impl HelperPolicy {
    pub fn bounded(interval: Option<u64>) -> Result<Self, &'static str> {
        let interval = interval.unwrap_or(1000);
        let delay = interval
            .checked_add(10_000)
            .ok_or("native_capability_unavailable")?
            .max(60_000);
        if interval == 0 || delay > 300_000 {
            return Err("native_capability_unavailable");
        }
        Ok(Self {
            refresh_interval_ms: interval,
            timeout_ms: 5000,
            max_apply_delay: delay,
        })
    }
}
/// Only the reserved fresh override opts in. Merely selecting a profile, a
/// provider name or a host helper never does so.
pub fn claude_helper_override(
    settings: &str,
    explicit_key_grant: bool,
    interval: Option<u64>,
) -> Result<Option<HelperPolicy>, &'static str> {
    #[derive(serde::Deserialize)]
    struct Settings {
        #[serde(rename = "apiKeyHelper")]
        helper: Option<String>,
    }
    let settings: Settings =
        serde_json::from_str(settings).map_err(|_| "native_capability_unavailable")?;
    if settings.helper.as_deref() != Some("/awman/credentials/helper") {
        return Ok(None);
    }
    if !explicit_key_grant {
        return Err("native_capability_unavailable");
    }
    HelperPolicy::bounded(interval).map(Some)
}
/// The selected native model provider owns the command callback. A competing
/// env key, bearer literal or ChatGPT auth requirement makes it unavailable.
pub fn codex_helper_override(
    config: &str,
    selected: &str,
    explicit_key_grant: bool,
) -> Result<Option<HelperPolicy>, &'static str> {
    #[derive(serde::Deserialize)]
    struct Config {
        #[serde(default)]
        model_providers: std::collections::BTreeMap<String, Provider>,
    }
    #[derive(serde::Deserialize)]
    struct Provider {
        auth: Option<Auth>,
        env_key: Option<String>,
        bearer_token: Option<String>,
        experimental_bearer_token: Option<String>,
        #[serde(default)]
        requires_openai_auth: bool,
    }
    #[derive(serde::Deserialize)]
    struct Auth {
        command: String,
        args: Option<Vec<String>>,
        refresh_interval_ms: Option<u64>,
        timeout_ms: Option<u64>,
    }
    let config: Config = toml::from_str(config).map_err(|_| "native_capability_unavailable")?;
    let Some(provider) = config.model_providers.get(selected) else {
        return Ok(None);
    };
    let Some(auth) = &provider.auth else {
        return Ok(None);
    };
    if auth.command != "/awman/credentials/helper" {
        return Ok(None);
    }
    if !explicit_key_grant
        || provider.env_key.is_some()
        || provider.bearer_token.is_some()
        || provider.experimental_bearer_token.is_some()
        || provider.requires_openai_auth
        || auth.args.as_ref().is_some_and(|a| !a.is_empty())
        || auth.timeout_ms.is_some_and(|n| n == 0 || n > 5000)
    {
        return Err("native_capability_unavailable");
    }
    HelperPolicy::bounded(auth.refresh_interval_ms).map(Some)
}
