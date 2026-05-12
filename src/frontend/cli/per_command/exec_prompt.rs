//! `ExecPromptCommandFrontend` impl for the CLI.
//!
//! `container_frontend()` comes from the blanket `HasContainerFrontend` impl
//! on `CliFrontend` (see `per_command/chat.rs`), which is a supertrait of
//! `ExecPromptCommandFrontend`.

use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;

impl ExecPromptCommandFrontend for CliFrontend {}
