//! `RemoteCommandFrontend` impl for the TUI.

use crate::command::commands::remote::RemoteCommandFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl RemoteCommandFrontend for TuiCommandFrontend {}
