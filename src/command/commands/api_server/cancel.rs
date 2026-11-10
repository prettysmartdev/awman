//! Cancelling an API command (WI 0135): the one owner of the cancel decision
//! table. Routes map a [`CancelOutcome`] to HTTP; they decide nothing.
//!
//! | Row when the request arrives | What happens                                   | Outcome            |
//! |------------------------------|------------------------------------------------|--------------------|
//! | none                         | nothing                                        | `NotFound`         |
//! | `done`/`error`/`cancelled`   | nothing                                        | `AlreadyTerminal`  |
//! | `queued`/`pending`           | row → `cancelled`; a `/v1/worker/runs` setup is told to stop | `Cancelled` |
//! | `running`                    | marker recorded, the owning worker's token fired | `Stopping`       |
//! | `running`, no owner          | (only between a crash and the next bootstrap) row → `cancelled` | `Cancelled` |
//! | `running`, no owner, sidecar | row → the recorded completion; an unreadable sidecar is an error | `AlreadyTerminal` |

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::command::error::CommandError;
use crate::data::api_lifecycle_store::CommandLifecycleStore;
use crate::data::command_status::CommandStatus;
use crate::data::fs::api_db::{CommandRecord, SqliteSessionStore};

/// In-process map from an id (command id, or session id for setups) to the
/// token the owning task observes. One instance per daemon for commands and
/// one for setups; both hang off `WorkerServices`.
#[derive(Default)]
pub struct TokenRegistry {
    tokens: Mutex<HashMap<String, CancellationToken>>,
}

impl TokenRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.tokens.lock().expect("token registry lock poisoned")
    }

    /// Create (or return the existing) token for `id`.
    pub fn register(&self, id: &str) -> CancellationToken {
        self.lock().entry(id.to_string()).or_default().clone()
    }

    pub fn unregister(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Fire the token for `id`. `false` when nothing is registered.
    pub fn cancel(&self, id: &str) -> bool {
        match self.lock().get(id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    pub fn cancel_all(&self) -> Vec<String> {
        let tokens = self.lock();
        tokens
            .iter()
            .map(|(id, token)| {
                token.cancel();
                id.clone()
            })
            .collect()
    }
    pub fn is_registered(&self, id: &str) -> bool {
        self.lock().contains_key(id)
    }

    /// Run `claim` and register a token for what it claimed under one lock,
    /// so [`TokenRegistry::cancel_or`] never sees a claimed command that has
    /// no token yet. The queue worker claims through this.
    pub fn claim<T, E>(
        &self,
        claim: impl FnOnce() -> Result<Option<T>, E>,
        id_of: impl FnOnce(&T) -> String,
    ) -> Result<Option<(T, CancellationToken)>, E> {
        let mut tokens = self.lock();
        let Some(claimed) = claim()? else {
            return Ok(None);
        };
        let token = tokens.entry(id_of(&claimed)).or_default().clone();
        Ok(Some((claimed, token)))
    }

    /// Fire the token for `id` and return `true`; when none is registered,
    /// run `unowned` while still holding the lock (so no claim can register
    /// in between) and return `false`.
    pub fn cancel_or<E>(
        &self,
        id: &str,
        unowned: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        let tokens = self.lock();
        match tokens.get(id) {
            Some(token) => {
                token.cancel();
                Ok(true)
            }
            None => {
                unowned()?;
                Ok(false)
            }
        }
    }
}

/// What a cancel request did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    NotFound,
    /// Already `done`/`error`/`cancelled`; carries the row so the route renders it.
    AlreadyTerminal(CommandRecord),
    /// Was queued (or running with no owner); is `cancelled` now. Carries the
    /// updated row.
    Cancelled(CommandRecord),
    /// Was running; the owning worker has been told to stop. The row stays
    /// `running` until the worker finalises it.
    Stopping {
        command_id: String,
        session_id: String,
        requested_at: String,
    },
}

/// The one owner of the cancel decision table. Routes map outcomes to HTTP.
#[derive(Clone)]
pub struct CommandCanceller {
    store: Arc<SqliteSessionStore>,
    lifecycle: CommandLifecycleStore,
    commands: Arc<TokenRegistry>,
    setups: Arc<TokenRegistry>,
    completion_paths: Option<crate::data::fs::api_paths::ApiPaths>,
}

impl CommandCanceller {
    pub fn new(
        store: Arc<SqliteSessionStore>,
        lifecycle: CommandLifecycleStore,
        commands: Arc<TokenRegistry>,
        setups: Arc<TokenRegistry>,
    ) -> Self {
        Self {
            store,
            lifecycle,
            commands,
            setups,
            completion_paths: None,
        }
    }

    pub fn with_completion_paths(mut self, paths: crate::data::fs::api_paths::ApiPaths) -> Self {
        self.completion_paths = Some(paths);
        self
    }

    fn finalize_unowned(&self, command_id: &str) -> Result<(), CommandError> {
        if let Some(paths) = &self.completion_paths {
            super::queue_worker::finalize_unowned_command(&self.store, paths, command_id)?;
        }
        Ok(())
    }

    /// A worker that could not store its completion has already published it;
    /// that outcome stands, whatever a later cancel asks for.
    fn finalize_from_sidecar(&self, command_id: &str) -> Result<bool, CommandError> {
        let Some(paths) = &self.completion_paths else {
            return Ok(false);
        };
        let Some(record) = self.store.get_command(command_id)? else {
            return Ok(false);
        };
        if record.status != CommandStatus::Running {
            return Ok(false);
        }
        Ok(super::queue_worker::finalize_running_from_sidecar(
            &self.store,
            paths,
            &record,
        )?)
    }

    pub fn cancel(&self, command_id: &str) -> Result<CancelOutcome, CommandError> {
        let Some(record) = self.store.get_command(command_id)? else {
            return Ok(CancelOutcome::NotFound);
        };
        if record.status.is_terminal() {
            return Ok(CancelOutcome::AlreadyTerminal(record));
        }

        let now = chrono::Utc::now().to_rfc3339();
        if record.status.is_claimable()
            && self.lifecycle.cancel_if_queued(
                &crate::data::ids::CommandId::parse(command_id)
                    .expect("validated persisted resource id"),
                &now,
            )?
        {
            // A `/v1/worker/runs` command waits in the queue while its session is
            // set up: abandon that setup too.
            if let Some(run) = self.lifecycle.run_for_command(
                &crate::data::ids::CommandId::parse(command_id)
                    .expect("validated persisted resource id"),
            )? {
                self.setups.cancel(run.session_id.as_str());
            }
            self.finalize_unowned(command_id)?;
            return Ok(CancelOutcome::Cancelled(self.reread(record)?));
        }

        // Running, or claimed by a worker since the read above.
        self.lifecycle.request_cancel(
            &crate::data::ids::CommandId::parse(command_id)
                .expect("validated persisted resource id"),
            &now,
        )?;
        let requested_at = self
            .lifecycle
            .cancel_requested_at(
                &crate::data::ids::CommandId::parse(command_id)
                    .expect("validated persisted resource id"),
            )?
            .unwrap_or(now.clone());
        let owned = self.commands.cancel_or(command_id, || {
            if !self.finalize_from_sidecar(command_id)? {
                self.lifecycle.cancel_if_running(
                    &crate::data::ids::CommandId::parse(command_id).expect("validated command id"),
                    &now,
                    &requested_at,
                )?;
            }
            Ok::<(), CommandError>(())
        })?;
        if !owned {
            let current = self.reread(record.clone())?;
            match current.status {
                CommandStatus::Cancelled => {
                    self.finalize_unowned(command_id)?;
                    return Ok(CancelOutcome::Cancelled(self.reread(current)?));
                }
                s if s.is_terminal() => return Ok(CancelOutcome::AlreadyTerminal(current)),
                // Not finalised (the row left `running` some other way):
                // the marker is recorded, so restart recovery cancels it.
                _ => {}
            }
        }
        Ok(CancelOutcome::Stopping {
            command_id: record.id,
            session_id: record.session_id,
            requested_at,
        })
    }

    /// The row as it is now, falling back to `record` if it vanished.
    fn reread(&self, record: CommandRecord) -> Result<CommandRecord, CommandError> {
        Ok(self.store.get_command(&record.id)?.unwrap_or(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (
        tempfile::TempDir,
        Arc<SqliteSessionStore>,
        CommandLifecycleStore,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(tmp.path()).unwrap());
        store
            .insert_session(
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "/work",
                "2026-10-28T00:00:00Z",
            )
            .unwrap();
        let lifecycle = CommandLifecycleStore::open(Arc::clone(&store)).unwrap();
        (tmp, store, lifecycle)
    }

    fn canceller(
        store: Arc<SqliteSessionStore>,
        lifecycle: CommandLifecycleStore,
        commands: Arc<TokenRegistry>,
    ) -> CommandCanceller {
        CommandCanceller::new(store, lifecycle, commands, Arc::new(TokenRegistry::new()))
    }

    #[test]
    fn queued_cancel_is_terminal_and_terminal_cancel_is_unchanged() {
        let (_tmp, store, lifecycle) = setup();
        store
            .enqueue_command(
                "cmd_11111111111141118111111111111111",
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "chat",
                "[]",
                "log",
            )
            .unwrap();
        let cancel = canceller(
            Arc::clone(&store),
            lifecycle,
            Arc::new(TokenRegistry::new()),
        );

        let CancelOutcome::Cancelled(row) = cancel
            .cancel("cmd_11111111111141118111111111111111")
            .unwrap()
        else {
            panic!("queued command should cancel immediately");
        };
        assert_eq!(row.status.as_str(), "cancelled");
        assert_eq!(
            row.exit_code,
            Some(crate::data::command_status::CANCELLED_EXIT_CODE)
        );
        assert!(store.claim_next_command("worker").unwrap().is_none());

        store
            .enqueue_command(
                "cmd_6b2ded51d81a4403d8a4bd25fa1e57ee",
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "chat",
                "[]",
                "log",
            )
            .unwrap();
        store
            .complete_command(
                "cmd_6b2ded51d81a4403d8a4bd25fa1e57ee",
                CommandStatus::Done,
                Some(0),
                Some(r#"{"exit_code":0}"#),
            )
            .unwrap();
        let before = store
            .get_command("cmd_6b2ded51d81a4403d8a4bd25fa1e57ee")
            .unwrap()
            .unwrap();
        assert!(matches!(
            cancel
                .cancel("cmd_6b2ded51d81a4403d8a4bd25fa1e57ee")
                .unwrap(),
            CancelOutcome::AlreadyTerminal(_)
        ));
        assert_eq!(
            store
                .get_command("cmd_6b2ded51d81a4403d8a4bd25fa1e57ee")
                .unwrap()
                .unwrap(),
            before
        );
    }

    #[test]
    fn running_cancel_fires_owner_token_and_restart_does_not_requeue_cancelled_row() {
        let (_tmp, store, lifecycle) = setup();
        store
            .enqueue_command(
                "cmd_75101dcdfc88455bcafc9e53e0b06689",
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "chat",
                "[]",
                "log",
            )
            .unwrap();
        let owner = Arc::new(TokenRegistry::new());
        let token = owner.register("cmd_75101dcdfc88455bcafc9e53e0b06689");
        let claimed = store.claim_next_command("worker").unwrap().unwrap();
        assert_eq!(claimed.status.as_str(), "running");
        let cancel = canceller(Arc::clone(&store), lifecycle.clone(), Arc::clone(&owner));

        assert!(matches!(
            cancel
                .cancel("cmd_75101dcdfc88455bcafc9e53e0b06689")
                .unwrap(),
            CancelOutcome::Stopping { .. }
        ));
        assert!(token.is_cancelled());
        assert_eq!(
            store
                .get_command("cmd_75101dcdfc88455bcafc9e53e0b06689")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "running"
        );

        // Model process death after the durable cancel marker and before the
        // worker finalises: bootstrap finalises it before stale recovery.
        let finalized = lifecycle
            .finalize_interrupted_cancellations("2026-10-28T00:01:00Z")
            .unwrap();
        assert_eq!(
            finalized.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
            ["cmd_75101dcdfc88455bcafc9e53e0b06689"]
        );
        assert!(store.recover_stale_commands(0).unwrap().is_empty());
        assert_eq!(
            store
                .get_command("cmd_75101dcdfc88455bcafc9e53e0b06689")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "cancelled"
        );
        assert!(store
            .claim_next_command("worker-after-restart")
            .unwrap()
            .is_none());
    }

    #[test]
    fn run_linkage_retains_close_on_finish_and_setup_failure_preserves_cancel() {
        let (_tmp, store, lifecycle) = setup();
        store
            .enqueue_command(
                "cmd_a53108f7543b75adbb34afc035d4cdf6",
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "exec workflow",
                "[]",
                "log",
            )
            .unwrap();
        lifecycle
            .record_run(&crate::data::api_lifecycle_store::RunRecord {
                session_id: crate::data::ids::WorkerSessionId::parse(
                    "ses_21d6f40cfb511982e4424e0e250a9557",
                )
                .unwrap(),
                command_id: crate::data::ids::CommandId::parse(
                    "cmd_a53108f7543b75adbb34afc035d4cdf6",
                )
                .unwrap(),
                close_on_finish: true,
                created_at: "2026-10-28T00:00:00Z".into(),
            })
            .unwrap();
        let run = lifecycle
            .run_for_command(
                &crate::data::ids::CommandId::parse("cmd_a53108f7543b75adbb34afc035d4cdf6")
                    .unwrap(),
            )
            .unwrap()
            .unwrap();
        assert!(run.close_on_finish);
        assert_eq!(lifecycle.list_runs().unwrap(), [run]);

        store
            .enqueue_command(
                "cmd_e2466d34c9b9eec2fe43009eb1f34df8",
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "exec workflow",
                "[]",
                "log",
            )
            .unwrap();
        lifecycle
            .cancel_if_queued(
                &crate::data::ids::CommandId::parse("cmd_e2466d34c9b9eec2fe43009eb1f34df8")
                    .unwrap(),
                "2026-10-28T00:00:01Z",
            )
            .unwrap();
        assert!(!lifecycle
            .fail_if_queued(
                "cmd_e2466d34c9b9eec2fe43009eb1f34df8",
                "2026-10-28T00:00:02Z",
                r#"{"error":"setup"}"#
            )
            .unwrap());
        assert_eq!(
            store
                .get_command("cmd_e2466d34c9b9eec2fe43009eb1f34df8")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "cancelled"
        );
    }
}
