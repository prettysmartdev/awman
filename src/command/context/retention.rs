//! Owned by workflow-scratch-push: scratch retention with command records.
use crate::data::context::CommandInvocationPins;
use crate::data::fs::{
    api_db::SqliteSessionStore, api_paths::ApiPaths, context_dirs::ContextDirResolver,
};
pub struct ScratchRetention {
    pins: CommandInvocationPins,
    resolver: ContextDirResolver,
}
impl ScratchRetention {
    pub fn new(paths: ApiPaths, resolver: ContextDirResolver) -> Self {
        Self {
            pins: CommandInvocationPins::new(paths),
            resolver,
        }
    }
    pub fn sweep_at_startup(&self, store: &SqliteSessionStore) -> usize {
        let result = self.sweep(store);
        match result {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(%error, "Workflow scratch retention sweep could not complete");
                0
            }
        }
    }

    fn sweep(&self, store: &SqliteSessionStore) -> Result<usize, crate::data::error::DataError> {
        // Startup calls this after delete_closed_sessions_older_than(24).
        // A closed session still present in SQLite retains all its scratch.
        let mut retained = std::collections::BTreeSet::new();
        let mut expired = Vec::new();
        for session in self.pins.session_dirs()? {
            let pins = match self.pins.all_for_session(&session) {
                Ok(pins) => pins,
                Err(error) => {
                    tracing::warn!(%error, "could not enumerate session scratch pins");
                    continue;
                }
            };
            if store.get_session(&session)?.is_some() {
                retained.extend(
                    pins.into_iter()
                        .flat_map(|(_, pin)| pin.previous.into_iter().chain([pin.invocation_id])),
                );
            } else {
                expired.push((session, pins));
            }
        }
        // A resumed invocation can be pinned by more than one command. Keep
        // its directory until the last referencing session has been purged.
        let mut removed = 0;
        for (session, pins) in expired {
            for (command, pin) in pins {
                let mut complete = true;
                for invocation in pin.previous.into_iter().chain([pin.invocation_id]) {
                    if retained.contains(&invocation) {
                        continue;
                    }
                    match self.resolver.remove_workflow_dir(invocation) {
                        Ok(()) => removed += 1,
                        Err(error) => {
                            complete = false;
                            tracing::warn!(%error, "Could not remove retained workflow scratch");
                        }
                    }
                }
                if complete {
                    if let Err(error) = self.pins.remove(&session, &command) {
                        tracing::warn!(%error, "Could not remove expired scratch pin");
                    }
                }
            }
        }
        Ok(removed)
    }
}
