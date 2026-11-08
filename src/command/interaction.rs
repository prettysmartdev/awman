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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::daemon::{
        DaemonConfig, SquadFeatureConfig, WorkerPullConfig, WorkerPushConfig,
    };

    fn defaults() -> DaemonSettings {
        DaemonSettings::from_merged(&DaemonConfig::default())
    }

    #[test]
    fn defaults_keep_todays_behaviour() {
        let settings = defaults();
        let squad = resolve_interaction(InteractionSlot::Squad, None, &settings).unwrap();
        assert_eq!(squad.interaction, Interaction::Interactive);
        assert_eq!(squad.pty_size, Some((120, 40)));
        let push = resolve_interaction(InteractionSlot::WorkerPush, None, &settings).unwrap();
        assert_eq!(push.interaction, Interaction::Headless);
        assert_eq!(push.pty_size, None, "headless runs stay piped");
        let pull = resolve_interaction(InteractionSlot::WorkerPull, None, &settings).unwrap();
        assert_eq!(pull.interaction, Interaction::Interactive);
    }

    #[test]
    fn an_allowed_override_wins() {
        let settings = defaults();
        let push = resolve_interaction(
            InteractionSlot::WorkerPush,
            Some(Interaction::Interactive),
            &settings,
        )
        .unwrap();
        assert_eq!(push.interaction, Interaction::Interactive);
        assert_eq!(push.pty_size, Some((120, 40)));
        let squad = resolve_interaction(
            InteractionSlot::Squad,
            Some(Interaction::Headless),
            &settings,
        )
        .unwrap();
        assert_eq!(squad.pty_size, None);
    }

    #[test]
    fn a_disallowed_override_is_refused_with_the_stable_code() {
        let mut settings = defaults();
        settings.worker.push_allow_override = false;
        let refused = resolve_interaction(
            InteractionSlot::WorkerPush,
            Some(Interaction::Interactive),
            &settings,
        )
        .unwrap_err();
        assert_eq!(refused.code(), "interaction_override_refused");
        assert_eq!(refused.key(), "daemon.features.worker.push.interaction");
        assert_eq!(refused.configured(), Interaction::Headless);
        assert_eq!(refused.requested(), Interaction::Interactive);
        // Asking for the configured mode is not an override.
        let same = resolve_interaction(
            InteractionSlot::WorkerPush,
            Some(Interaction::Headless),
            &settings,
        )
        .unwrap();
        assert_eq!(same.interaction, Interaction::Headless);
        // No request is never refused.
        assert!(resolve_interaction(InteractionSlot::WorkerPush, None, &settings).is_ok());
    }

    /// `api.alwaysNonInteractive`'s effect is now
    /// `worker.push = { interaction: headless, allowOverride: false }`.
    #[test]
    fn push_locked_headless_matches_the_removed_always_non_interactive() {
        let settings = DaemonSettings::from_merged(&DaemonConfig {
            features: Some(crate::data::config::daemon::FeaturesConfig {
                worker: Some(crate::data::config::daemon::WorkerFeatureConfig {
                    push: Some(WorkerPushConfig {
                        interaction: Some(Interaction::Headless),
                        allow_override: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(resolve_interaction(
            InteractionSlot::WorkerPush,
            Some(Interaction::Interactive),
            &settings
        )
        .is_err());
    }

    /// A pulled item uses `worker.pull.interaction` unless its
    /// `exec.interaction` overrides it and overrides are allowed.
    #[test]
    fn pull_uses_its_setting_unless_the_item_overrides() {
        let settings = DaemonSettings::from_merged(&DaemonConfig {
            features: Some(crate::data::config::daemon::FeaturesConfig {
                worker: Some(crate::data::config::daemon::WorkerFeatureConfig {
                    pull: Some(WorkerPullConfig {
                        interaction: Some(Interaction::Headless),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        let plain = resolve_interaction(InteractionSlot::WorkerPull, None, &settings).unwrap();
        assert_eq!(plain.interaction, Interaction::Headless);
        let item = resolve_interaction(
            InteractionSlot::WorkerPull,
            Some(Interaction::Interactive),
            &settings,
        )
        .unwrap();
        assert_eq!(item.interaction, Interaction::Interactive);
        let mut locked = settings.clone();
        locked.worker.pull_allow_override = false;
        let refused = resolve_interaction(
            InteractionSlot::WorkerPull,
            Some(Interaction::Interactive),
            &locked,
        )
        .unwrap_err();
        assert_eq!(refused.key(), "daemon.features.worker.pull.interaction");
    }

    #[test]
    fn squad_override_honours_its_allow_override() {
        let settings = DaemonSettings::from_merged(&DaemonConfig {
            features: Some(crate::data::config::daemon::FeaturesConfig {
                squad: Some(SquadFeatureConfig {
                    interaction: Some(Interaction::Headless),
                    allow_override: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        let run = resolve_interaction(InteractionSlot::Squad, None, &settings).unwrap();
        assert_eq!(run.interaction, Interaction::Headless);
        assert_eq!(run.pty_size, None);
        assert!(resolve_interaction(
            InteractionSlot::Squad,
            Some(Interaction::Interactive),
            &settings
        )
        .is_err());
    }

    #[test]
    fn the_pty_size_comes_from_daemon_terminal() {
        let settings = DaemonSettings::from_merged(&DaemonConfig {
            terminal: Some(crate::data::config::daemon::DaemonTerminalConfig {
                cols: Some(200),
                rows: Some(60),
                ..Default::default()
            }),
            ..Default::default()
        });
        for slot in [
            InteractionSlot::Squad,
            InteractionSlot::WorkerPush,
            InteractionSlot::WorkerPull,
        ] {
            let decision =
                resolve_interaction(slot, Some(Interaction::Interactive), &settings).unwrap();
            assert_eq!(decision.pty_size, Some((200, 60)), "{slot:?}");
        }
        assert_eq!(
            decision_for(Interaction::Interactive, &settings).headless_fallback(),
            InteractionDecision {
                interaction: Interaction::Headless,
                pty_size: None
            }
        );
    }

    #[test]
    fn flags_become_a_request() {
        assert_eq!(requested_from_flags(None, None), None);
        assert_eq!(requested_from_flags(Some(false), Some(false)), None);
        assert_eq!(
            requested_from_flags(Some(true), None),
            Some(Interaction::Interactive)
        );
        assert_eq!(
            requested_from_flags(None, Some(true)),
            Some(Interaction::Headless)
        );
        assert_eq!(
            requested_from_flags(None, Some(false)),
            Some(Interaction::Interactive)
        );
        assert_eq!(
            requested_from_flags(Some(false), None),
            Some(Interaction::Headless)
        );
    }
}
