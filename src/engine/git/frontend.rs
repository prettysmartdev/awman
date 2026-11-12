//! `GitFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! The logged git methods used to compose the `$ git <args>` echo line
//! themselves and push it through `UserMessageSink` (WI 0114 F-45: Layer 1
//! rendering transcript text). The engine now reports the *fact* — this
//! command is about to run — and the frontend decides how to draw it.
//!
//! The default impl writes the identical line the engine wrote before, so no
//! frontend's output changes until it overrides the method.

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

/// A git invocation the engine is about to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommand {
    /// Arguments after the `git` binary, in order.
    pub args: Vec<String>,
}

impl GitCommand {
    pub fn new(args: &[&str]) -> Self {
        Self {
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    /// The command as a user would type it: `git status --porcelain`.
    pub fn display(&self) -> String {
        format!("git {}", self.args.join(" "))
    }
}

impl std::fmt::Display for GitCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// Layer 0 cannot name a Layer 1 trait, so the impl for Layer 0's test
/// recorder lives here. It takes the default `command_started`, so a test
/// that uses the recorder as a git log sink records the same `$ git …` line
/// the engine wrote before F-45.
impl GitFrontend for crate::data::message::RecordingMessageSink {}

/// What the logged git methods report to.
pub trait GitFrontend: UserMessageSink {
    /// The engine is about to run this git command.
    ///
    /// The default writes `$ git <args>` as an info message — byte-identical
    /// to what `run_git_logged` emitted before F-45 — so a frontend that does
    /// not override this sees no change. A frontend that wants to render the
    /// command differently (a dimmed prompt line, a collapsible group) can
    /// override and emit nothing here.
    fn command_started(&mut self, command: &GitCommand) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("$ {command}"),
        });
    }
}
