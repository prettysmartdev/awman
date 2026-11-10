//! Foreground queue policy. Presentation supplies selections and a Dispatch frontend.
use super::queue_source::{CheckoutChoice, PickRun, SharedQueueSource};
use crate::command::{
    daemon_api::worker_pull::QueueRow, dispatch::BuildContext, error::CommandError,
};
use crate::data::{prompt::Prompt, session::Session};

pub trait QueueFrontend: Send + Sync {
    fn pick_item(&mut self, items: &[QueueRow]) -> Result<Option<String>, CommandError>;
    fn confirm_checkout(&mut self, prompt: &Prompt<CheckoutChoice>) -> CheckoutChoice;
    fn input_available(&self) -> bool;
}

pub struct QueueCommand {
    pub session: Session,
    pub server: Option<String>,
    pub json: bool,
    engines: crate::command::dispatch::Engines,
}
impl QueueCommand {
    pub fn new(
        session: Session,
        engines: crate::command::dispatch::Engines,
        server: Option<String>,
        json: bool,
    ) -> Self {
        Self {
            session,
            engines,
            server,
            json,
        }
    }
    pub fn from_input(ctx: &BuildContext<'_>) -> Result<Self, CommandError> {
        Ok(Self {
            session: ctx.session.clone(),
            engines: ctx.engines.clone(),
            server: ctx.flags.string("server"),
            json: ctx.flags.bool("json"),
        })
    }
    pub async fn list(&self) -> Result<crate::data::wire::List<QueueRow>, CommandError> {
        use crate::command::commands::daemon::{actions::Request, client::DaemonClient};
        if let Some(client) = DaemonClient::discover(self.session.env())? {
            let mut request = Request {
                method: "GET".into(),
                path: "/v1/worker/queue".into(),
                query: vec![],
                body: None,
            };
            if let Some(name) = &self.server {
                request.query.push(("server".into(), name.clone()));
            }
            let response = client.all(&request).await;
            let daemon_off = matches!(&response, Err(CommandError::DaemonHttpStatus { status: 503, body })
                if serde_json::from_str::<serde_json::Value>(body).ok().is_some_and(|value| matches!(value["code"].as_str(), Some("feature_disabled" | "pull_disabled"))));
            if !daemon_off {
                let items = response?
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<Result<_, _>>()
                    .map_err(|e| CommandError::Other(e.to_string()))?;
                return Ok(crate::data::wire::List {
                    items,
                    next_cursor: None,
                });
            }
        }
        let source =
            super::tui_host::TuiWorkerController::start_manual(&self.session, &self.engines)
                .map_err(|e| CommandError::Other(e.to_string()))?;
        self.rows_from_source(&source)
    }
    fn rows_from_source(
        &self,
        source: &SharedQueueSource,
    ) -> Result<crate::data::wire::List<QueueRow>, CommandError> {
        let snapshot = source.snapshot();
        if let Some(error) = snapshot.error {
            return Err(CommandError::Other(error));
        }
        let items = snapshot
            .items
            .into_iter()
            .filter(|item| self.server.as_ref().is_none_or(|s| s == &item.server))
            .map(|item| QueueRow {
                body: item.body,
                source_url: item.source_url,
                id: item.id,
                server: item.server,
                title: item.title,
                summary: item.summary,
                kind: if item.kind == "prompt" {
                    awman_wsp::WorkKind::Prompt
                } else {
                    awman_wsp::WorkKind::Workflow
                },
                labels: item.labels,
                priority: item.priority,
                age_ms: item.age_ms,
                enqueued_at: chrono::Utc::now()
                    - chrono::Duration::milliseconds(item.age_ms.min(i64::MAX as u64) as i64),
                for_you: item.for_you,
            })
            .collect();
        Ok(crate::data::wire::List {
            items,
            next_cursor: None,
        })
    }
}

pub struct QueuePickCommand {
    pub queue: QueueCommand,
    pub item_id: Option<String>,
    prepared: Option<(SharedQueueSource, PickRun)>,
    cancel: tokio_util::sync::CancellationToken,
}
impl QueuePickCommand {
    pub fn new(
        session: Session,
        engines: crate::command::dispatch::Engines,
        server: Option<String>,
        item_id: Option<String>,
        json: bool,
    ) -> Self {
        Self {
            queue: QueueCommand::new(session, engines, server, json),
            item_id,
            prepared: None,
            cancel: Default::default(),
        }
    }
    pub fn from_input(ctx: &BuildContext<'_>) -> Result<Self, CommandError> {
        Ok(Self {
            queue: QueueCommand::from_input(ctx)?,
            item_id: ctx.args.get("item-id").map(str::to_string),
            prepared: None,
            cancel: ctx.cancel.clone(),
        })
    }
    pub fn prepared(
        mut self,
        source: SharedQueueSource,
        run: PickRun,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        self.prepared = Some((source, run));
        self.cancel = cancel;
        self
    }
    /// Frontends retain the source for the run's lifetime and use its prepared
    /// session in Dispatch. The command line contains no private item body.
    pub async fn prepare(
        &self,
        frontend: &mut dyn QueueFrontend,
    ) -> Result<(SharedQueueSource, PickRun), CommandError> {
        if let Some((source, run)) = &self.prepared {
            let mut run = run.clone();
            if let Some(prompt) = source.checkout_offer(&run.lease_id) {
                if frontend.confirm_checkout(&prompt) == CheckoutChoice::Worktree {
                    run = source
                        .choose_checkout(&run.lease_id, CheckoutChoice::Worktree)
                        .map_err(|e| {
                            let _ = source.finish_lease(
                                &run.lease_id,
                                super::queue_source::LeaseCloseChoice::Release,
                            );
                            CommandError::Other(format!("queue checkout: {e:?}"))
                        })?;
                }
            }
            return Ok((source.clone(), run));
        }
        if self.item_id.is_none() && (!frontend.input_available() || self.queue.json) {
            return Err(CommandError::MissingRequiredArgument {
                command: vec!["queue".into(), "pick".into()],
                argument: "item-id (required when stdin is not a TTY or --json is set)".into(),
            });
        }
        // One host owns listing, picking and execution. Creating a temporary
        // list host first would register a second standalone worker.
        let source = super::tui_host::TuiWorkerController::start_manual(
            &self.queue.session,
            &self.queue.engines,
        )
        .map_err(|e| CommandError::Other(e.to_string()))?;
        let rows = self.queue.rows_from_source(&source)?.items;
        let id = match &self.item_id {
            Some(id) => id.clone(),
            None => frontend.pick_item(&rows)?.ok_or(CommandError::Aborted)?,
        };
        let row = rows
            .iter()
            .find(|row| row.id == id && self.queue.server.as_ref().is_none_or(|s| s == &row.server))
            .ok_or_else(|| {
                CommandError::Other(format!(
                    "queue item {id:?} is no longer eligible; refresh the queue"
                ))
            })?;
        let mut run = source
            .pick(&row.server, &id)
            .map_err(|e| CommandError::Other(format!("queue pick: {e:?}")))?;
        if let Some(prompt) = source.checkout_offer(&run.lease_id) {
            let choice = if self.queue.json {
                CheckoutChoice::FreshClone
            } else {
                frontend.confirm_checkout(&prompt)
            };
            if choice == CheckoutChoice::Worktree {
                run = source.choose_checkout(&run.lease_id, choice).map_err(|e| {
                    let _ = source.finish_lease(
                        &run.lease_id,
                        super::queue_source::LeaseCloseChoice::Release,
                    );
                    CommandError::Other(format!("queue checkout: {e:?}"))
                })?;
            }
        }
        Ok((source, run))
    }
}

/// A checkout offer is made only from the actual repository origin.
pub fn checkout_offer(
    session: &Session,
    engines: &crate::command::dispatch::Engines,
    item: &awman_wsp::WorkItem,
) -> Option<Prompt<CheckoutChoice>> {
    engines
        .git_engine
        .remote_url(session.git_root(), "origin")
        .ok()
        .filter(|origin| crate::data::worker::paths::same_remote(origin, &item.repo.url))
        .map(|_| super::queue_source::checkout_prompt(&item.title))
}

/// A presentation factory supplies the caller's ordinary command frontend;
/// Layer 2 retains Dispatch, lease ownership, cancellation and result policy.
pub trait QueueRunFrontendFactory: QueueFrontend {
    type Frontend: crate::command::dispatch::DispatchFrontend;
    fn command_frontend(
        &mut self,
        input: crate::command::dispatch::parsed_input::ParsedCommandBoxInput,
        events: crate::command::commands::api_server::event_bus::EventBusSender,
        writer: crate::data::fs::api_command_log::CommandStateWriter,
    ) -> Result<Self::Frontend, CommandError>;
}

/// Reuse the caller's frontend, preserving dialogs and terminal channels.
pub struct DispatchQueueFrontend<F>(Option<F>);
impl<F> DispatchQueueFrontend<F> {
    pub fn new(frontend: F) -> Self {
        Self(Some(frontend))
    }
}
impl<F: crate::command::dispatch::DispatchFrontend> QueueFrontend for DispatchQueueFrontend<F> {
    fn pick_item(&mut self, items: &[QueueRow]) -> Result<Option<String>, CommandError> {
        self.0.as_mut().unwrap().pick_queue_item(items)
    }
    fn confirm_checkout(&mut self, prompt: &Prompt<CheckoutChoice>) -> CheckoutChoice {
        self.0.as_mut().unwrap().confirm_queue_checkout(prompt)
    }
    fn input_available(&self) -> bool {
        self.0.as_ref().unwrap().input_available()
    }
}
impl<F: crate::command::dispatch::DispatchFrontend> QueueRunFrontendFactory
    for DispatchQueueFrontend<F>
{
    type Frontend = F;
    fn command_frontend(
        &mut self,
        input: crate::command::dispatch::ParsedCommandBoxInput,
        events: crate::command::commands::api_server::event_bus::EventBusSender,
        writer: crate::data::fs::api_command_log::CommandStateWriter,
    ) -> Result<F, CommandError> {
        let mut frontend = self.0.take().expect("one picked execution");
        frontend.bind_worker_run(input, events, writer)?;
        Ok(frontend)
    }
}
impl QueuePickCommand {
    pub async fn run_with_frontend<F: QueueRunFrontendFactory>(
        self,
        mut frontend: F,
    ) -> Result<crate::data::fs::api_db::CommandResult, CommandError> {
        let (source, run) = self.prepare(&mut frontend).await?;
        Self::run_prepared_cancellable(source, run, &mut frontend, self.cancel).await
    }
    pub async fn run_prepared<F: QueueRunFrontendFactory>(
        source: SharedQueueSource,
        run: PickRun,
        frontend: &mut F,
    ) -> Result<crate::data::fs::api_db::CommandResult, CommandError> {
        Self::run_prepared_cancellable(source, run, frontend, Default::default()).await
    }
    async fn run_prepared_cancellable<F: QueueRunFrontendFactory>(
        source: SharedQueueSource,
        run: PickRun,
        frontend: &mut F,
        owner_cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::data::fs::api_db::CommandResult, CommandError> {
        let result =
            Self::execute_prepared(source.clone(), run.clone(), frontend, owner_cancel).await;
        if result.is_err() {
            let _ = tokio::task::spawn_blocking(move || {
                // Before launch this releases immediately. After launch it
                // latches cancellation, then completes with persisted partials.
                let _ = source.finish_lease(
                    &run.lease_id,
                    super::queue_source::LeaseCloseChoice::Release,
                );
                if let Some(writer) = source.writer_for_lease(&run.lease_id) {
                    let partial =
                        crate::command::commands::api_server::queue_worker::foreground_result(
                            &Err(CommandError::Aborted),
                            writer.result_contributions(),
                        );
                    let _ = source.complete_lease(&run.lease_id, partial);
                }
            })
            .await;
        }
        result
    }
    async fn execute_prepared<F: QueueRunFrontendFactory>(
        source: SharedQueueSource,
        run: PickRun,
        frontend: &mut F,
        owner_cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::data::fs::api_db::CommandResult, CommandError> {
        use crate::command::dispatch::{parsed_input::ParsedCommandBoxInput, Dispatch};
        let missing =
            || CommandError::Other("foreground lease lost its prepared command context".into());
        let session = source
            .session_for_lease(&run.lease_id)
            .ok_or_else(missing)?;
        let mut engines = source
            .engines_for_lease(&run.lease_id)
            .ok_or_else(missing)?;
        let bus = source.events_for_lease(&run.lease_id).ok_or_else(missing)?;
        let cancel = source
            .cancellation_for_lease(&run.lease_id)
            .ok_or_else(missing)?;
        let writer = source.writer_for_lease(&run.lease_id).ok_or_else(missing)?;
        let stop_bridge = tokio_util::sync::CancellationToken::new();
        let _stop_bridge = stop_bridge.clone().drop_guard();
        let lease_cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop_bridge.cancelled() => {},
                _ = owner_cancel.cancelled() => lease_cancel.cancel(),
                _ = lease_cancel.cancelled() => owner_cancel.cancel(),
            }
        });
        let catalogue = crate::command::dispatch::catalogue::CommandCatalogue::get();
        let mut tokens = shell_words::split(&run.command_line)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        // Prompt input stays private in the session and in memory. It is not
        // part of PickRun.command_line, JobSpec, the row, or process argv.
        if tokens.starts_with(&["exec".into(), "prompt".into()]) {
            if let Some(path) = session.inline_work_item() {
                use std::io::Read;
                let mut body = String::new();
                crate::data::fs::private_files::open_private_file(path)?
                    .read_to_string(&mut body)
                    .map_err(|e| CommandError::Other(format!("private prompt input: {e}")))?;
                tokens.extend(["--".into(), body]);
            }
        }
        let line = shell_words::join(tokens.iter().map(String::as_str));
        let input = crate::command::dispatch::parsed_input::parse(&line, catalogue)?;
        let path = input.path.clone();
        let input: ParsedCommandBoxInput = input;
        let command_frontend = frontend.command_frontend(input, bus.sender(), writer.clone())?;
        source
            .begin_lease(&run.lease_id)
            .map_err(CommandError::Other)?;
        let checkout_root = session.git_root().to_path_buf();
        let git = engines.git_engine.clone();
        let base = git.command_base_sha(&checkout_root);
        engines.runtime = std::sync::Arc::new(
            crate::command::commands::api_server::owned_runtime::CommandOwnedRuntime::new(
                engines.runtime.clone(),
                writer.command_id(),
            )
            .with_owner(crate::data::terminal::TerminalOwnerInfo {
                owner: crate::data::terminal::TerminalOwner::Command(
                    crate::data::ids::CommandId::parse(writer.command_id())
                        .map_err(|e| CommandError::Other(e.to_string()))?,
                ),
                feature: crate::data::daemon_state::FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: source.terminal_input_for_lease(&run.lease_id),
            })
            .with_output_writer(writer.clone()),
        );
        let dispatch = Dispatch::new(
            command_frontend,
            std::sync::Arc::new(tokio::sync::RwLock::new(session)),
            engines,
        )
        .with_cancellation(cancel.clone())
        .with_output_dir(writer.output_dir());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::runtime::Handle::current();
        let command_path = path.clone();
        std::thread::Builder::new()
            .name("awman-foreground-command".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                use futures_util::FutureExt;
                let result = handle.block_on(
                    std::panic::AssertUnwindSafe(async move {
                        let refs: Vec<_> = command_path.iter().map(String::as_str).collect();
                        dispatch.run_command(&refs).await
                    })
                    .catch_unwind(),
                );
                let result = result.unwrap_or_else(|_| {
                    Err(CommandError::Other("foreground command panicked".into()))
                });
                let _ = tx.send(result);
            })
            .map_err(|e| CommandError::Other(e.to_string()))?;
        let outcome = rx.await.map_err(|e| CommandError::Other(e.to_string()))?;
        crate::command::commands::api_server::queue_worker::wait_for_output_drains(
            &bus,
            &run.lease_id,
        )
        .await;
        let mut result = crate::command::commands::api_server::queue_worker::foreground_result(
            &outcome,
            writer.result_contributions(),
        );
        let root = writer.kept_worktree().unwrap_or(checkout_root);
        let git_result =
            tokio::task::spawn_blocking(move || git.command_result(&root, base.as_deref()))
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
        let refs: Vec<_> = path.iter().map(String::as_str).collect();
        crate::command::commands::api_server::queue_worker::finish_result_fields(
            &mut result,
            &outcome,
            &refs,
            git_result,
            &writer.output_dir(),
        );
        if cancel.is_cancelled() {
            result.outcome = Some(crate::data::execution_event::CommandStatusKind::Cancelled);
            result.exit_code = Some(130);
        }
        tokio::task::spawn_blocking(move || source.complete_lease(&run.lease_id, result))
            .await
            .map_err(|e| CommandError::Other(e.to_string()))?
            .map_err(CommandError::Other)
    }
}
