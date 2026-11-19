//! Automation mode, status assurance and turn outcomes — Layer 0.
//!
//! `--automation[=auto|require_full]` asks awman to own an execution's turn
//! lifecycle and to report how much it could verify. These enums are the one
//! spelling of that contract for flags, `flags_applied`, `automation_state`,
//! turn records, events and command results. They carry no policy: which
//! tier a launch reaches, and whether a task may be delivered, is decided in
//! Layer 2 from Layer 1 observations.

use serde::{Deserialize, Serialize};

/// The catalogue flag (`--automation`) and its `flags_applied` key.
pub const AUTOMATION_FLAG: &str = "automation";

/// `/v1/status` capabilities and event features this contract advertises.
/// A step adds a name to the advertised set only when the operation works.
pub const AGENT_CAPABILITIES_CAPABILITY: &str = "agent_capabilities";
pub const AGENT_AUTOMATION_CAPABILITY: &str = "agent_automation";
pub const TERMINAL_TURNS_CAPABILITY: &str = "terminal_turns";
pub const AUTOMATION_RESULTS_CAPABILITY: &str = "automation_results";
pub const AGENT_TURN_RECORDS_CAPABILITY: &str = "agent_turn_records";

/// Event names persisted in the existing command event stream.
pub const AGENT_STATE_EVENT: &str = "agent_state";
pub const AGENT_IDENTITY_EVENT: &str = "agent_identity";
pub const AGENT_TURN_STARTED_EVENT: &str = "agent_turn_started";
pub const AGENT_TURN_COMPLETED_EVENT: &str = "agent_turn_completed";

/// The guest directory of the execution-private lifecycle observation
/// channel (versioned JSONL written by awman-authored guest hooks).
pub const AUTOMATION_GUEST_DIR: &str = "/awman/automation";

/// A `submit_turn` prompt's size bound, in UTF-8 bytes.
pub const MAX_TURN_PROMPT_BYTES: usize = 64 * 1024;

/// `--automation`'s value. Omitting the flag disables automation; a bare
/// `--automation` is `auto`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationMode {
    /// Prefer full native status; fall back to OSC 7501 automatically.
    #[default]
    Auto,
    /// Require full native status before every automated task delivery.
    RequireFull,
}

impl AutomationMode {
    pub const ALL: [Self; 2] = [Self::Auto, Self::RequireFull];

    /// The accepted explicit values, in the order help lists them. The
    /// catalogue's `--automation` spec is checked against this.
    pub const VALUES: &'static [&'static str] = &["auto", "require_full"];

    /// The value a bare `--automation` means.
    pub const MISSING_VALUE: &'static str = "auto";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::RequireFull => "require_full",
        }
    }

    /// Parse an explicit flag value. Empty and unknown values are `None`.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == value)
    }

    /// The admission floor this mode snapshots at launch.
    pub fn minimum_status_support(self) -> MinimumStatusSupport {
        match self {
            Self::Auto => MinimumStatusSupport::BestEffort,
            Self::RequireFull => MinimumStatusSupport::Full,
        }
    }
}

/// The lowest status tier at which a launch permits automated task
/// delivery. Immutable for the launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MinimumStatusSupport {
    Full,
    BestEffort,
}

impl MinimumStatusSupport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::BestEffort => "best_effort",
        }
    }

    /// Whether `current` meets this floor. `none` never does.
    pub fn is_met_by(self, current: StatusSupport) -> bool {
        match (self, current) {
            (_, StatusSupport::None) => false,
            (Self::Full, StatusSupport::BestEffort) => false,
            (Self::Full, StatusSupport::Full) => true,
            (Self::BestEffort, StatusSupport::Full | StatusSupport::BestEffort) => true,
        }
    }
}

/// How well an execution's lifecycle can currently be observed.
///
/// Ordered weakest first, so `min` of two tiers is the weaker one (the
/// "weakest tier used" a turn or command result reports).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSupport {
    /// No usable source: admission is unknown/unavailable.
    None,
    /// Decoded OSC 7501 reports and qualified prompt delivery.
    BestEffort,
    /// The complete native lifecycle contract.
    Full,
}

impl StatusSupport {
    pub const ALL: [Self; 3] = [Self::None, Self::BestEffort, Self::Full];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BestEffort => "best_effort",
            Self::Full => "full",
        }
    }

    /// The weaker of two tiers.
    pub fn weakest(self, other: Self) -> Self {
        self.min(other)
    }
}

/// Where an execution's current status evidence comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    Native,
    Osc7501,
    None,
}

impl StatusSource {
    pub const ALL: [Self; 3] = [Self::Native, Self::Osc7501, Self::None];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Osc7501 => "osc7501",
            Self::None => "none",
        }
    }
}

/// An automation execution's lifecycle state (`automation_state.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationState {
    Starting,
    Unknown,
    Ready,
    Submitting,
    Working,
    BlockedAuth,
    BlockedPermission,
    BlockedInput,
    CredentialUpdating,
    Restarting,
    Stopping,
    Ended,
}

impl AutomationState {
    pub const ALL: [Self; 12] = [
        Self::Starting,
        Self::Unknown,
        Self::Ready,
        Self::Submitting,
        Self::Working,
        Self::BlockedAuth,
        Self::BlockedPermission,
        Self::BlockedInput,
        Self::CredentialUpdating,
        Self::Restarting,
        Self::Stopping,
        Self::Ended,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Unknown => "unknown",
            Self::Ready => "ready",
            Self::Submitting => "submitting",
            Self::Working => "working",
            Self::BlockedAuth => "blocked_auth",
            Self::BlockedPermission => "blocked_permission",
            Self::BlockedInput => "blocked_input",
            Self::CredentialUpdating => "credential_updating",
            Self::Restarting => "restarting",
            Self::Stopping => "stopping",
            Self::Ended => "ended",
        }
    }
}

/// One turn's immutable outcome (`turn_result.result.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// A correlated native terminal result confirmed completion.
    Completed,
    /// OSC-only `done`: reported, not verified.
    CompletedUnverified,
    Failed,
    Cancelled,
    /// A crash between reservation and observed consumption. Never retried.
    DeliveryUnknown,
}

impl TurnStatus {
    pub const ALL: [Self; 5] = [
        Self::Completed,
        Self::CompletedUnverified,
        Self::Failed,
        Self::Cancelled,
        Self::DeliveryUnknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::CompletedUnverified => "completed_unverified",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::DeliveryUnknown => "delivery_unknown",
        }
    }
}

/// Which source established readiness for a turn (`readiness_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessSource {
    Native,
    Osc7501,
}

/// Which source established a turn's completion (`completion_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionSource {
    Native,
    Osc7501,
    None,
}

/// Whether output capture and drain finished (`output_status`). Separate
/// from whether the agent's completion was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStatus {
    Complete,
    Incomplete,
}

/// Whether owned resources were confirmed removed (`cleanup_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStatus {
    Complete,
    Pending,
    Failed,
}

/// Where a capability statement came from (`agent-capabilities.evidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityEvidence {
    Declaration,
    Probe,
    Observed,
    None,
}

/// Which startup clock a launch uses. Ordinary launches keep the legacy
/// first-byte grace kill; automation launches disable it in every bridge and
/// rely on the absolute `agents.automation.startupTimeout` lifecycle deadline
/// (output, runtime chatter or a first byte never reset or satisfy it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StartupTiming {
    #[default]
    FirstByteGrace,
    LifecycleDeadline,
}

/// The automation adapter a definition selects. First-party definitions name
/// their own; a custom definition names one in `[automation].adapter`, and
/// omitting it declares automation and native resume unsupported. There is
/// deliberately no `gemini` adapter, and none is guessed from an executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AutomationAdapterKind {
    #[serde(rename = "claude")]
    Claude,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "opencode")]
    Opencode,
    #[serde(rename = "opencode-v2")]
    OpencodeV2,
    #[serde(rename = "cursor")]
    Cursor,
    #[serde(rename = "copilot")]
    Copilot,
    #[serde(rename = "cline")]
    Cline,
    #[serde(rename = "antigravity")]
    Antigravity,
    /// Best-effort interactive status only: no native identity, exact resume
    /// or structured headless result.
    #[serde(rename = "osc7501")]
    Osc7501,
}

impl AutomationAdapterKind {
    pub const ALL: [Self; 9] = [
        Self::Claude,
        Self::Codex,
        Self::Opencode,
        Self::OpencodeV2,
        Self::Cursor,
        Self::Copilot,
        Self::Cline,
        Self::Antigravity,
        Self::Osc7501,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::OpencodeV2 => "opencode-v2",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
            Self::Cline => "cline",
            Self::Antigravity => "antigravity",
            Self::Osc7501 => "osc7501",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|adapter| adapter.as_str() == value)
    }

    /// Whether the adapter can claim native identity, exact resume and
    /// structured results at all. The generic OSC adapter cannot.
    pub fn is_provider_adapter(self) -> bool {
        self != Self::Osc7501
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: Serialize>(value: T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn automation_mode_values_and_floor() {
        assert_eq!(AutomationMode::default(), AutomationMode::Auto);
        assert_eq!(
            AutomationMode::parse(AutomationMode::MISSING_VALUE),
            Some(AutomationMode::Auto)
        );
        let spelled: Vec<_> = AutomationMode::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(spelled, AutomationMode::VALUES);
        for mode in AutomationMode::ALL {
            assert_eq!(wire(mode), mode.as_str());
            assert_eq!(AutomationMode::parse(mode.as_str()), Some(mode));
        }
        for bad in ["", "full", "Auto", "require-full", "true"] {
            assert_eq!(AutomationMode::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            wire(AutomationMode::Auto.minimum_status_support()),
            "best_effort"
        );
        assert_eq!(
            wire(AutomationMode::RequireFull.minimum_status_support()),
            "full"
        );
    }

    #[test]
    fn best_effort_never_meets_a_full_floor_and_none_meets_nothing() {
        use MinimumStatusSupport as Min;
        use StatusSupport as Now;
        assert!(Min::Full.is_met_by(Now::Full));
        assert!(!Min::Full.is_met_by(Now::BestEffort));
        assert!(!Min::Full.is_met_by(Now::None));
        assert!(Min::BestEffort.is_met_by(Now::Full));
        assert!(Min::BestEffort.is_met_by(Now::BestEffort));
        assert!(!Min::BestEffort.is_met_by(Now::None));
    }

    #[test]
    fn weakest_tier_is_the_minimum() {
        assert_eq!(
            StatusSupport::Full.weakest(StatusSupport::BestEffort),
            StatusSupport::BestEffort
        );
        assert_eq!(
            StatusSupport::BestEffort.weakest(StatusSupport::None),
            StatusSupport::None
        );
        assert_eq!(
            StatusSupport::Full.weakest(StatusSupport::Full),
            StatusSupport::Full
        );
    }

    #[test]
    fn wire_spellings_match_as_str() {
        for value in StatusSupport::ALL {
            assert_eq!(wire(value), value.as_str());
        }
        for value in StatusSource::ALL {
            assert_eq!(wire(value), value.as_str());
        }
        for value in AutomationState::ALL {
            assert_eq!(wire(value), value.as_str());
        }
        for value in TurnStatus::ALL {
            assert_eq!(wire(value), value.as_str());
        }
        assert_eq!(wire(ReadinessSource::Osc7501), "osc7501");
        assert_eq!(wire(CompletionSource::None), "none");
        assert_eq!(wire(OutputStatus::Incomplete), "incomplete");
        assert_eq!(wire(CleanupStatus::Pending), "pending");
        assert_eq!(wire(CapabilityEvidence::Declaration), "declaration");
    }

    #[test]
    fn adapters_cover_the_eight_agents_plus_osc_and_no_gemini() {
        let names: Vec<_> = AutomationAdapterKind::ALL
            .iter()
            .map(|a| a.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "claude",
                "codex",
                "opencode",
                "opencode-v2",
                "cursor",
                "copilot",
                "cline",
                "antigravity",
                "osc7501"
            ]
        );
        for adapter in AutomationAdapterKind::ALL {
            assert_eq!(wire(adapter), adapter.as_str());
            assert_eq!(
                AutomationAdapterKind::parse(adapter.as_str()),
                Some(adapter)
            );
            assert_eq!(
                adapter.is_provider_adapter(),
                adapter != AutomationAdapterKind::Osc7501
            );
        }
        assert_eq!(AutomationAdapterKind::parse("gemini"), None);
        assert!(serde_json::from_str::<AutomationAdapterKind>("\"gemini\"").is_err());
    }
}
