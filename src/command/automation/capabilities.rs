//! Owner-local capability declarations and turn pages (WI 0070 B9).
//!
//! Queries never launch or contact a provider: they return the selected
//! adapters' declarations (support fields `null` until runtime evidence)
//! filtered by the admitted session.

use crate::data::automation_records::AutomationCapabilities;
use crate::data::session::Session;
use crate::engine::agent::automation::adapter_for;

/// Declarations for every definition visible to `session` on `runtime`.
pub fn agent_capabilities(session: &Session, runtime: &str) -> Vec<AutomationCapabilities> {
    use crate::engine::agent::registry::AgentRegistry;
    let osc = session.effective_config().program_status_enabled();
    let mut out = Vec::new();
    let listing = AgentRegistry::list(session.env(), Some(session.definition_root()));
    for name in listing.names() {
        let Ok(agent) = crate::data::session::AgentName::new(name.clone()) else {
            continue;
        };
        let Ok(definition) = AgentRegistry::resolve_for_session(session, &agent) else {
            continue;
        };
        match adapter_for(&definition) {
            Some(adapter) => out.push(adapter.declarations(&definition, runtime, osc)),
            None => out.push(unsupported(&definition, runtime)),
        }
    }
    out
}

fn unsupported(
    definition: &crate::engine::agent::AgentDefinition,
    runtime: &str,
) -> AutomationCapabilities {
    use crate::data::automation::{CapabilityEvidence, StatusSupport};
    use crate::data::automation_records::*;
    let reason = Some("the definition declares no automation adapter".to_string());
    AutomationCapabilities {
        agent: definition.name.clone(),
        runtime: runtime.to_owned(),
        provider_version: None,
        image_digest: None,
        minimum_version: None,
        version_warning: None,
        evidence: CapabilityEvidence::Declaration,
        native_plan: NativePlanCapability {
            interactive: Some(definition.native_plan.is_some()),
            headless: Some(
                definition
                    .native_plan
                    .as_ref()
                    .is_some_and(|p| p.headless_argv.is_some()),
            ),
            acp: Some(
                definition
                    .native_plan
                    .as_ref()
                    .is_some_and(|p| p.acp_mode_id.is_some()),
            ),
            kind: definition.native_plan.as_ref().map(|p| p.kind),
        },
        headless_turns: SupportCapability {
            supported: Some(false),
            reason: reason.clone(),
        },
        terminal_turns: TerminalTurnsCapability {
            supported: Some(false),
            status_support: Some(StatusSupport::None),
            sources: vec![],
            reason: reason.clone(),
        },
        native_resume: NativeResumeCapability {
            interactive: Some(false),
            headless: Some(false),
            reason,
        },
        settings_profiles: definition.profile_state.is_some(),
        credential_updates: crate::engine::agent::credential_routes::declarations(definition),
    }
}
