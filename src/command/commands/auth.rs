//! `AuthCommand` — accept/decline keychain consent for the current repo.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct AuthCommandFlags {
    pub accept: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthOutcome {
    pub accepted: bool,
    /// `true` when the choice was persisted to the per-repo config.
    #[serde(default)]
    pub persisted: bool,
}

pub trait AuthCommandFrontend: UserMessageSink + Send + Sync {
    /// Prompt the user for [y/n/o]nce. CLI implementations gate on stdin TTY
    /// and return `accept` as a safe default when not a TTY.
    fn ask_consent(&mut self, default: bool) -> Result<AuthConsentChoice, CommandError> {
        Ok(if default {
            AuthConsentChoice::Accept
        } else {
            AuthConsentChoice::Decline
        })
    }
}

/// Tri-state user choice for agent auth consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthConsentChoice {
    Accept,
    Decline,
    Once,
}

pub struct AuthCommand {
    flags: AuthCommandFlags,
    engines: Engines,
    session: crate::data::session::Session,
}

impl AuthCommand {
    pub fn new(
        flags: AuthCommandFlags,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            flags,
            engines,
            session,
        }
    }

    pub fn flags(&self) -> &AuthCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for AuthCommand {
    type Frontend = Box<dyn AuthCommandFrontend>;
    type Outcome = AuthOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let choice = frontend.ask_consent(self.flags.accept)?;
        let (accepted, persist) = match choice {
            AuthConsentChoice::Accept => (true, true),
            AuthConsentChoice::Decline => (false, true),
            AuthConsentChoice::Once => (true, false),
        };
        let mut persisted = false;
        if persist {
            // Persist on the per-repo config so future agent launches respect
            // the choice without re-prompting.
            {
                let session = &self.session;
                let mut cfg = session.repo_config().clone();
                cfg.auto_agent_auth_accepted = Some(accepted);
                if cfg.save(session.git_root()).is_ok() {
                    persisted = true;
                }
            }
        }
        frontend.replay_queued();
        Ok(AuthOutcome {
            accepted,
            persisted,
        })
    }
}
