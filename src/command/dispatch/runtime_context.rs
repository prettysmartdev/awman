//! `RuntimeContext` — the Layer 1 graph and the session, assembled once.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::dispatch::Engines;
use crate::data::session::Session;

/// Bundle of state the binary constructs once at startup and hands to
/// whichever frontend it is about to run.
///
/// The same engines and session are reused regardless of which frontend runs;
/// only the `Dispatch` wrapper differs. That is precisely why it is not a
/// frontend's type: it lived in `src/frontend/cli/mod.rs`, so `src/frontend/tui`
/// had to import from `src/frontend/cli` to name it (WI 0114 F-23).
///
/// Distinct from `StartupOutcome`, which is what `Startup` *produces* —
/// session, engines, a possible fatal runtime error and the startup messages.
/// This is the subset a running frontend needs once those have been dealt
/// with, with the session already shared.
pub struct RuntimeContext {
    pub session: Arc<RwLock<Session>>,
    pub engines: Engines,
}

impl RuntimeContext {
    pub fn new(session: Session, engines: Engines) -> Self {
        Self {
            session: Arc::new(RwLock::new(session)),
            engines,
        }
    }
}
