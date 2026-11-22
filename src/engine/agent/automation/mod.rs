//! Definition-selected automation adapters — Layer 1 (WI 0070).
//!
//! An [`AgentAutomationAdapter`] is chosen only by the definition's
//! [`AutomationAdapterKind`] (`AgentDefinition::automation_adapter`); no
//! frontend or command matches a provider name. Adapters supply:
//!
//! * static capability declarations (never a launch or provider contact);
//! * task-only prompt validation and whole-operation encoding;
//! * which native lifecycle source the route has (the execution-private
//!   guest channel decoded by [`observer`]) — the generic OSC adapter has
//!   none; OSC 7501 itself is decoded by [`osc7501`].
//!
//! Readiness, tiers and admission policy live in Layer 2. Terminal rendering
//! bytes are never an adapter input: there is no screen recognizer.

pub mod observer;
pub mod osc7501;

use crate::data::agent_output_format::AgentOutputFormat;
use crate::data::automation::{
    AutomationAdapterKind, CapabilityEvidence, ReadinessSource, MAX_TURN_PROMPT_BYTES,
};
use crate::data::automation_records::{
    AutomationCapabilities, NativePlanCapability, NativeResumeCapability, SupportCapability,
    TerminalTurnsCapability,
};
use crate::engine::agent::AgentDefinition;

/// Bound on one encoded write operation.
pub const MAX_ENCODED_PROMPT_BYTES: usize = 256 * 1024;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const SUBMIT: &[u8] = b"\r";

/// Why a task cannot be delivered as provider task text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptRefusal {
    /// Empty, too long, control characters or a command shortcut: the
    /// request is invalid (`invalid_request`).
    #[error("{0}")]
    Invalid(String),
    /// The route has no safe delivery operation
    /// (`native_capability_unavailable`).
    #[error("{0}")]
    Unavailable(String),
}

/// One whole, writer-owned input operation. Never split into a prefix and
/// a separately queued submit key.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedPrompt {
    bytes: Vec<u8>,
    digest: String,
}

impl std::fmt::Debug for EncodedPrompt {
    /// Never prints prompt text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedPrompt")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl EncodedPrompt {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
    /// SHA-256 of the task text (hex), for correlating a native echo
    /// without retaining or logging the text.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// How a route delivers a guarded task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    /// Bracketed paste of the whole task followed by one submit key, as one
    /// writer-owned operation. Pasted LF stays task text.
    BracketedPaste,
    /// No safe delivery operation is established on this route.
    Unavailable,
}

/// The native lifecycle source a route can provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLifecycle {
    /// The execution-private guest channel (`/awman/automation`). Full
    /// support still requires runtime facility evidence and fresh records.
    GuestChannel,
    /// None: only OSC 7501 can establish best-effort status.
    None,
}

/// A definition-selected automation adapter.
pub trait AgentAutomationAdapter: Send + Sync {
    fn kind(&self) -> AutomationAdapterKind;

    /// Leading characters (after leading Unicode whitespace) that the
    /// provider interprets as a command or shell shortcut rather than task
    /// text. A guarded submission starting with one is refused.
    fn command_shortcuts(&self) -> &'static [char];

    fn prompt_delivery(&self) -> PromptDelivery {
        PromptDelivery::BracketedPaste
    }

    fn native_lifecycle(&self) -> NativeLifecycle {
        if self.kind().is_provider_adapter() {
            NativeLifecycle::GuestChannel
        } else {
            NativeLifecycle::None
        }
    }

    /// Validate and encode one complete task. Never normalizes content to
    /// conceal a shortcut.
    fn encode_prompt(&self, task: &str) -> Result<EncodedPrompt, PromptRefusal> {
        validate_task(task, self.command_shortcuts())?;
        match self.prompt_delivery() {
            PromptDelivery::Unavailable => Err(PromptRefusal::Unavailable(format!(
                "the {} adapter has no established task delivery on this route",
                self.kind().as_str()
            ))),
            PromptDelivery::BracketedPaste => {
                let mut bytes =
                    Vec::with_capacity(task.len() + PASTE_START.len() + PASTE_END.len() + 1);
                bytes.extend_from_slice(PASTE_START);
                bytes.extend_from_slice(task.as_bytes());
                bytes.extend_from_slice(PASTE_END);
                bytes.extend_from_slice(SUBMIT);
                if bytes.len() > MAX_ENCODED_PROMPT_BYTES {
                    return Err(PromptRefusal::Invalid(
                        "the encoded task exceeds its bound".into(),
                    ));
                }
                Ok(EncodedPrompt {
                    bytes,
                    digest: prompt_digest(task),
                })
            }
        }
    }

    /// Static declarations for one definition on one runtime. Support
    /// fields stay `null` (unknown) until runtime evidence exists: a
    /// declaration describes an implemented adapter operation, not an
    /// observed ready execution.
    fn declarations(
        &self,
        definition: &AgentDefinition,
        runtime: &str,
        osc_enabled: bool,
    ) -> AutomationCapabilities {
        let provider = self.kind().is_provider_adapter();
        let structured = definition.output_format != AgentOutputFormat::Text;
        let mut sources = Vec::new();
        if self.native_lifecycle() == NativeLifecycle::GuestChannel {
            sources.push(ReadinessSource::Native);
        }
        if osc_enabled && self.prompt_delivery() == PromptDelivery::BracketedPaste {
            sources.push(ReadinessSource::Osc7501);
        }
        let terminal_turns = if sources.is_empty() {
            TerminalTurnsCapability {
                supported: Some(false),
                status_support: Some(crate::data::automation::StatusSupport::None),
                sources,
                reason: Some(if osc_enabled {
                    "no native lifecycle source or task delivery on this route".into()
                } else {
                    "agents.programStatus.enabled=false disables the OSC 7501 source and this adapter has no native lifecycle source".into()
                }),
            }
        } else {
            TerminalTurnsCapability {
                supported: None,
                status_support: None,
                sources,
                reason: Some(
                    "established at startup from the execution's own lifecycle records".into(),
                ),
            }
        };
        let resume = definition.native_resume.is_some() && provider;
        let plan = definition.native_plan.as_ref();
        AutomationCapabilities {
            agent: definition.name.clone(),
            runtime: runtime.to_owned(),
            provider_version: None,
            image_digest: None,
            minimum_version: None,
            version_warning: None,
            evidence: CapabilityEvidence::Declaration,
            native_plan: NativePlanCapability {
                interactive: Some(plan.is_some_and(|p| {
                    p.interactive_argv.is_some() || p.interactive_settings.is_some()
                })),
                headless: Some(plan.is_some_and(|p| p.headless_argv.is_some())),
                acp: Some(plan.is_some_and(|p| p.acp_mode_id.is_some())),
                kind: plan.map(|p| p.kind),
            },
            headless_turns: SupportCapability {
                supported: Some(provider && structured),
                reason: (!(provider && structured)).then(|| {
                    if provider {
                        "the definition has no structured one-shot output".into()
                    } else {
                        "the generic osc7501 adapter has no structured headless result".into()
                    }
                }),
            },
            terminal_turns,
            native_resume: NativeResumeCapability {
                interactive: Some(resume),
                headless: Some(resume && structured),
                reason: (!resume).then(|| {
                    if provider {
                        "the definition declares no exact native resume selector".into()
                    } else {
                        "the generic osc7501 adapter has no native identity or exact resume".into()
                    }
                }),
            },
            settings_profiles: definition.profile_state.is_some(),
            credential_updates: crate::engine::agent::credential_routes::declarations(definition),
        }
    }
}

/// SHA-256 hex of a task text.
pub fn prompt_digest(task: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(task.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Layer-0 style task validation shared by every adapter: nonblank, at most
/// 64 KiB, no control characters other than LF/TAB, no C1/DEL, no Unicode
/// line/paragraph separators, and no leading command shortcut.
pub fn validate_task(task: &str, shortcuts: &[char]) -> Result<(), PromptRefusal> {
    if task.trim().is_empty() {
        return Err(PromptRefusal::Invalid("the task is empty".into()));
    }
    if task.len() > MAX_TURN_PROMPT_BYTES {
        return Err(PromptRefusal::Invalid(format!(
            "the task exceeds {MAX_TURN_PROMPT_BYTES} bytes"
        )));
    }
    if let Some(c) = task.chars().find(|c| {
        let c = *c;
        (c.is_control() && c != '\n' && c != '\t') || c == '\u{2028}' || c == '\u{2029}'
    }) {
        return Err(PromptRefusal::Invalid(format!(
            "the task contains a disallowed control character U+{:04X}",
            c as u32
        )));
    }
    let leading = task.trim_start();
    if let Some(first) = leading.chars().next() {
        if shortcuts.contains(&first) {
            return Err(PromptRefusal::Invalid(format!(
                "a task cannot start with `{first}`: it would run a provider command, not a model task"
            )));
        }
    }
    Ok(())
}

struct Adapter {
    kind: AutomationAdapterKind,
    shortcuts: &'static [char],
}

impl AgentAutomationAdapter for Adapter {
    fn kind(&self) -> AutomationAdapterKind {
        self.kind
    }
    fn command_shortcuts(&self) -> &'static [char] {
        self.shortcuts
    }
    /// The first-party interactive entrypoints implement bracketed paste, so
    /// a whole task (embedded newlines included) is one submission. The
    /// generic adapter fronts an arbitrary OSC-reporting program whose
    /// paste handling is unknown: an OSC readiness report says nothing
    /// about safe delivery, so guarded submission is refused there.
    fn prompt_delivery(&self) -> PromptDelivery {
        if self.kind.is_provider_adapter() {
            PromptDelivery::BracketedPaste
        } else {
            PromptDelivery::Unavailable
        }
    }
}

static ADAPTERS: [Adapter; 9] = [
    // Claude: slash commands, `!` bash mode, `#` memory.
    Adapter {
        kind: AutomationAdapterKind::Claude,
        shortcuts: &['/', '!', '#'],
    },
    Adapter {
        kind: AutomationAdapterKind::Codex,
        shortcuts: &['/', '!'],
    },
    Adapter {
        kind: AutomationAdapterKind::Opencode,
        shortcuts: &['/', '!'],
    },
    Adapter {
        kind: AutomationAdapterKind::OpencodeV2,
        shortcuts: &['/', '!'],
    },
    Adapter {
        kind: AutomationAdapterKind::Cursor,
        shortcuts: &['/', '!'],
    },
    Adapter {
        kind: AutomationAdapterKind::Copilot,
        shortcuts: &['/', '!'],
    },
    Adapter {
        kind: AutomationAdapterKind::Cline,
        shortcuts: &['/', '!'],
    },
    Adapter {
        kind: AutomationAdapterKind::Antigravity,
        shortcuts: &['/', '!'],
    },
    // Generic: conservative slash and shell shortcuts.
    Adapter {
        kind: AutomationAdapterKind::Osc7501,
        shortcuts: &['/', '!'],
    },
];

/// The adapter a kind selects.
pub fn adapter(kind: AutomationAdapterKind) -> &'static dyn AgentAutomationAdapter {
    ADAPTERS
        .iter()
        .find(|a| a.kind == kind)
        .expect("every adapter kind has an implementation")
}

/// The adapter a definition selects, or `None` when it declares none.
pub fn adapter_for(definition: &AgentDefinition) -> Option<&'static dyn AgentAutomationAdapter> {
    definition.automation_adapter.map(adapter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_one_adapter_and_first_party_definitions_select_theirs() {
        for kind in AutomationAdapterKind::ALL {
            assert_eq!(adapter(kind).kind(), kind);
        }
        for name in [
            "claude",
            "codex",
            "opencode",
            "opencode-v2",
            "cursor",
            "copilot",
            "cline",
            "antigravity",
        ] {
            let def = crate::engine::agent::first_party::first_party(name).unwrap();
            assert_eq!(adapter_for(&def).unwrap().kind().as_str(), name);
        }
    }

    #[test]
    fn encoding_is_one_bracketed_operation_and_preserves_internal_text() {
        let a = adapter(AutomationAdapterKind::Codex);
        let e = a
            .encode_prompt("Review the diff\n/not a command here\tok")
            .unwrap();
        assert!(e.bytes().starts_with(PASTE_START));
        assert!(e.bytes().ends_with(b"\x1b[201~\r"));
        assert_eq!(
            e.digest(),
            prompt_digest("Review the diff\n/not a command here\tok")
        );
        assert!(!format!("{e:?}").contains("Review"));
    }

    #[test]
    fn task_validation_refuses_shortcuts_controls_and_bounds() {
        let a = adapter(AutomationAdapterKind::Claude);
        for bad in [
            "",
            "   \n\t",
            "/model opus",
            "  \u{3000}/clear",
            "!rm -rf /",
            "# remember",
            "a\rb",
            "a\x1b[201~b",
            "a\0b",
            "a\u{85}b",
            "a\u{7f}b",
            "a\u{2028}b",
        ] {
            assert!(
                matches!(a.encode_prompt(bad), Err(PromptRefusal::Invalid(_))),
                "{bad:?}"
            );
        }
        let big = "x".repeat(MAX_TURN_PROMPT_BYTES + 1);
        assert!(a.encode_prompt(&big).is_err());
        assert!(a.encode_prompt("Explain --model and /tmp paths").is_ok());
        // The generic adapter also refuses shell shortcuts.
        assert!(adapter(AutomationAdapterKind::Osc7501)
            .encode_prompt("!ls")
            .is_err());
    }

    #[test]
    fn declarations_are_unknown_until_observed_and_never_full_by_declaration() {
        let def = crate::engine::agent::first_party::first_party("claude").unwrap();
        let caps = adapter_for(&def)
            .unwrap()
            .declarations(&def, "docker", true);
        assert_eq!(caps.evidence, CapabilityEvidence::Declaration);
        assert_eq!(caps.terminal_turns.supported, None);
        assert_eq!(caps.terminal_turns.status_support, None);
        assert_eq!(
            caps.terminal_turns.sources,
            vec![ReadinessSource::Native, ReadinessSource::Osc7501]
        );
        assert_eq!(caps.headless_turns.supported, Some(true));
        assert_eq!(caps.native_resume.interactive, Some(true));
        let generic = adapter(AutomationAdapterKind::Osc7501).declarations(&def, "docker", false);
        assert_eq!(generic.terminal_turns.supported, Some(false));
        assert_eq!(generic.native_resume.interactive, Some(false));
        assert_eq!(generic.headless_turns.supported, Some(false));
        // OSC readiness alone never establishes delivery for an unknown
        // program: even with reporting enabled, guarded turns are refused.
        let generic = adapter(AutomationAdapterKind::Osc7501).declarations(&def, "docker", true);
        assert_eq!(generic.terminal_turns.supported, Some(false));
        assert!(matches!(
            adapter(AutomationAdapterKind::Osc7501).encode_prompt("one\ntwo"),
            Err(PromptRefusal::Unavailable(_))
        ));
    }
}
