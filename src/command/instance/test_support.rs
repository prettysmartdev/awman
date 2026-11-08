//! Test fixtures for the instance command frontend (moved with it from the
//! TUI test module, WI 0142).

use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::projections::raw_args::ParsedArgs;
use crate::command::instance::feeds::TabCommandFeeds;
use crate::command::instance::frontend::{DialogChannels, InstanceCommandFrontend};
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::engine::agent_runtime::frontend::AgentIo;

/// Build an [`InstanceCommandFrontend`] for a unit test, plus the two dialog
/// endpoints the test drives it from.
///
/// This is the **only** place an instance unit test hand-builds a
/// [`ParsedCommandBoxInput`].
pub(crate) fn test_command_frontend(
    path: &[&str],
    args: ParsedArgs,
) -> (
    InstanceCommandFrontend,
    std::sync::mpsc::Receiver<DialogRequest>,
    std::sync::mpsc::Sender<DialogResponse>,
) {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
    let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
    let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    let (stderr_tx, _stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let container_io = AgentIo {
        stdout: stdout_tx,
        stderr: stderr_tx,
        stdin_tx,
        stdin_rx,
        resize: Some(resize_rx),
        initial_size: Some((80, 24)),
    };
    let frontend = InstanceCommandFrontend::new(
        ParsedCommandBoxInput {
            path: path.iter().map(|s| s.to_string()).collect(),
            args,
        },
        DialogChannels::new(req_tx, resp_rx),
        container_io,
        TabCommandFeeds::for_tests(),
    );
    (frontend, req_rx, resp_tx)
}
