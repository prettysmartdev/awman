//! Interaction resolution (WI 0141): how an agent runs — in a PTY
//! (`interactive`) or piped (`headless`) — is a setting of each daemon
//! feature, overridable per command or work item where the feature allows.
//!
//! This is the one place the rule lives. The push admission path
//! (`command::commands::api_server::admission`), the squad evaluation
//! (`command::commands::squad::evaluation`) and 0143's pull worker all call
//! [`resolve_interaction`]; frontends only build the `AgentIo` the decision
//! asks for. `engine` has no setting: it is always interactive.

use crate::data::config::daemon::{DaemonSettings, Interaction};

/// Error code for a refused override (`400` on the API).
pub const INTERACTION_OVERRIDE_REFUSED: &str = "interaction_override_refused";

/// Which feature setting governs a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InteractionSlot {
    /// `daemon.features.squad.interaction` (default `interactive`).
    Squad,
    /// `daemon.features.worker.push.interaction` (default `headless`).
    WorkerPush,
    /// `daemon.features.worker.pull.interaction` (default `interactive`).
    WorkerPull,
}

impl InteractionSlot {
    /// The config key path, for messages.
    pub fn key(self) -> &'static str {
        match self {
            Self::Squad => "daemon.features.squad.interaction",
            Self::WorkerPush => "daemon.features.worker.push.interaction",
            Self::WorkerPull => "daemon.features.worker.pull.interaction",
        }
    }

    /// The configured default and whether overrides are allowed.
    pub fn configured(self, settings: &DaemonSettings) -> (Interaction, bool) {
        match self {
            Self::Squad => (settings.squad.interaction, settings.squad.allow_override),
            Self::WorkerPush => (
                settings.worker.push_interaction,
                settings.worker.push_allow_override,
            ),
            Self::WorkerPull => (
                settings.worker.pull_interaction,
                settings.worker.pull_allow_override,
            ),
        }
    }
}

/// What a run gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionDecision {
    pub interaction: Interaction,
    /// `Some((cols, rows))` from `daemon.terminal.{cols,rows}` for an
    /// interactive unattended run, so the daemon frontend sets
    /// `AgentIo::initial_size` and the runtime takes the PTY path. `None`
    /// for headless.
    pub pty_size: Option<(u16, u16)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InteractionRefused {
    /// The request asked for a mode other than the configured one while the
    /// feature's `allowOverride` is `false`.
    #[error("{key} is {} and overrides are disabled", configured.as_str())]
    OverrideDisabled {
        key: &'static str,
        configured: Interaction,
        requested: Interaction,
    },
}

impl InteractionRefused {
    pub fn code(&self) -> &'static str {
        INTERACTION_OVERRIDE_REFUSED
    }

    /// The setting that refused, e.g. `daemon.features.worker.push.interaction`.
    pub fn key(&self) -> &'static str {
        match self {
            Self::OverrideDisabled { key, .. } => key,
        }
    }

    pub fn configured(&self) -> Interaction {
        match self {
            Self::OverrideDisabled { configured, .. } => *configured,
        }
    }

    pub fn requested(&self) -> Interaction {
        match self {
            Self::OverrideDisabled { requested, .. } => *requested,
        }
    }
}

/// Decide a run's interaction: the request's choice when the feature allows
/// overrides, else the configured default; a disallowed override is refused
/// at submit, never silently downgraded.
pub fn resolve_interaction(
    slot: InteractionSlot,
    requested: Option<Interaction>,
    settings: &DaemonSettings,
) -> Result<InteractionDecision, InteractionRefused> {
    let (configured, allow_override) = slot.configured(settings);
    let interaction = match requested {
        Some(requested) if requested != configured && !allow_override => {
            return Err(InteractionRefused::OverrideDisabled {
                key: slot.key(),
                configured,
                requested,
            });
        }
        Some(requested) => requested,
        None => configured,
    };
    Ok(decision_for(interaction, settings))
}

/// The decision for an already-chosen mode: an interactive run gets its PTY
/// size from `daemon.terminal.{cols,rows}` as they are now (read at spawn,
/// so running terminals keep the size they started with).
pub fn decision_for(interaction: Interaction, settings: &DaemonSettings) -> InteractionDecision {
    let pty_size = match interaction {
        Interaction::Interactive => Some((settings.terminal.cols, settings.terminal.rows)),
        Interaction::Headless => None,
    };
    InteractionDecision {
        interaction,
        pty_size,
    }
}

impl InteractionDecision {
    /// Downgrade to piped I/O when the run cannot have a PTY (an ACP-launched
    /// agent or an agent without PTY support). The run then has no terminal;
    /// clients follow its event stream instead.
    pub fn headless_fallback(self) -> Self {
        Self {
            interaction: Interaction::Headless,
            pty_size: None,
        }
    }

    pub fn is_headless(&self) -> bool {
        self.interaction.is_headless()
    }
}

/// Turn a command's `--interactive` / `--headless` flags into a request.
/// Neither flag means "no preference". `--interactive` wins if a caller
/// somehow sets both to `true` (the catalogue declares them in conflict).
pub fn requested_from_flags(
    interactive: Option<bool>,
    headless: Option<bool>,
) -> Option<Interaction> {
    match (interactive, headless) {
        (Some(true), _) => Some(Interaction::Interactive),
        (_, Some(true)) => Some(Interaction::Headless),
        (Some(false), None) => Some(Interaction::Headless),
        (None, Some(false)) => Some(Interaction::Interactive),
        _ => None,
    }
}
