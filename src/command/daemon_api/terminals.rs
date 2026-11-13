//! `/v1/terminals` resource operations (WI 0141): the Layer 2 face the HTTP
//! transport and `awman daemon api terminals.list` both call.

use std::sync::Arc;

use crate::command::terminals::{TerminalOperations, TerminalRefusal, TerminalSession};
use crate::data::ids::{TaskId, TerminalId};
use crate::data::terminal::{TerminalOwner, TerminalRecord, ViewerIdentity};
use crate::data::wire::{List, ListQuery};

/// `GET /v1/terminals?owner_id=…` → `{items, next_cursor}`.
///
/// `owner_id` is a `cmd_`, `tab_`, `lease_` or `run_` id, or a squad
/// `task_` id, which lists its active run's terminals (none when the task
/// is not running). Without `owner_id`, every terminal the viewer may see.
pub fn terminals_list(
    ops: &TerminalOperations,
    owner_id: Option<&str>,
    query: &ListQuery,
    viewer: &ViewerIdentity,
) -> Result<List<TerminalRecord>, TerminalRefusal> {
    let owner = match owner_id {
        None => None,
        Some(raw) => match TaskId::parse(raw) {
            Ok(task) => match ops.owner_for_task(&task) {
                Some(owner) => Some(owner),
                None => {
                    // Validate the page anyway so a bad cursor is still a 400.
                    query.page()?;
                    return Ok(List {
                        items: Vec::new(),
                        next_cursor: None,
                    });
                }
            },
            Err(_) => Some(
                TerminalOwner::parse(raw)
                    .map_err(|_| TerminalRefusal::InvalidOwner(raw.to_owned()))?,
            ),
        },
    };
    ops.list(owner.as_ref(), query, viewer)
}

/// `GET /v1/terminals/{id}` without an upgrade → the record.
pub fn terminals_get(
    ops: &TerminalOperations,
    id: &TerminalId,
    viewer: &ViewerIdentity,
) -> Result<TerminalRecord, TerminalRefusal> {
    ops.get(id, viewer)
}

/// `GET /v1/terminals/{id}` with `Upgrade: websocket` → an admitted session
/// the transport drives.
pub fn terminals_open(
    ops: &Arc<TerminalOperations>,
    id: &TerminalId,
    viewer: ViewerIdentity,
) -> Result<TerminalSession, TerminalRefusal> {
    ops.open(id, viewer)
}
