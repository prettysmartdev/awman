//! Resumable command events, independent of their presentation or transport.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::data::execution_event::{EventCursor, EventPayload, ExecutionEvent, ReplayFrom};
use crate::data::fs::api_db::{CommandRecord, SqliteSessionStore};
use crate::data::fs::api_paths::ApiPaths;

use super::event_bus::EventBus;

/// Delivery facts needed by adapters that preserve resumable cursors and
/// progress notices. Transient events deliberately have no resumable cursor.
pub enum CommandLogUpdate {
    Event {
        event: Box<ExecutionEvent>,
        cursor: Option<EventCursor>,
    },
    Restart(u32),
    Lagged(u64),
    /// Whether progress currently depends on a worker claiming the command.
    Waiting(bool),
}

impl CommandLogUpdate {
    fn persisted(event: ExecutionEvent) -> Self {
        let cursor = Some(event.cursor());
        Self::Event {
            event: Box::new(event),
            cursor,
        }
    }

    fn transient(event: ExecutionEvent) -> Self {
        Self::Event {
            event: Box::new(event),
            cursor: None,
        }
    }
}

struct CommandLogSource {
    store: Arc<SqliteSessionStore>,
    paths: ApiPaths,
    event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
}

/// One subscription. Dropping it cancels its replay/polling task, without
/// affecting the execution, its bus, or another subscriber.
pub struct CommandLogStream {
    receiver: tokio::sync::mpsc::Receiver<CommandLogUpdate>,
    task: tokio::task::JoinHandle<()>,
}

impl CommandLogStream {
    pub async fn open(
        store: Arc<SqliteSessionStore>,
        paths: ApiPaths,
        event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
        command: CommandRecord,
        requested: Option<EventCursor>,
        step_filter: Option<String>,
        poll: Duration,
    ) -> Self {
        let state = CommandLogSource {
            store,
            paths,
            event_buses,
        };
        let (tx, receiver) = tokio::sync::mpsc::channel(256);
        let task = tokio::spawn(async move {
            // Subscribe before reading disk and deduplicate the overlap. If the
            // writer is behind this subscription, repair the gap before forwarding.
            let command_id = command.id.clone();
            let session_id = command.session_id.clone();
            let mut live_rx = state
                .event_buses
                .lock()
                .await
                .get(&command_id)
                .map(|bus| bus.subscribe());
            let events = command_log_events(&state, &session_id, &command_id).await;
            let replay_from = ReplayFrom::resolve(&events, command.attempt, requested);
            let mut cursor =
                CommandLogCursor::new(replay_from, command.attempt).with_step_filter(step_filter);
            if let ReplayFrom::StartOfAttempt(attempt) = replay_from {
                if tx.send(CommandLogUpdate::Restart(attempt)).await.is_err() {
                    return;
                }
            }
            if !cursor.replay(&events, &tx).await {
                return;
            }
            if command.status.is_terminal() {
                finish_terminal_log(
                    &state,
                    &session_id,
                    &command_id,
                    &mut cursor,
                    &mut live_rx,
                    &tx,
                )
                .await;
                return;
            }

            let mut saw_worker = live_rx.is_some();
            if tx
                .send(CommandLogUpdate::Waiting(live_rx.is_none()))
                .await
                .is_err()
            {
                return;
            }
            loop {
                if live_rx.is_none() {
                    // This informational event is outside the persisted cursor
                    // namespace. An id-less frame leaves Last-Event-ID untouched.
                    let waiting = ExecutionEvent {
                        timestamp: chrono::Utc::now(),
                        attempt: 0,
                        sequence: 0,
                        source: None,
                        payload: EventPayload::StatusMessage {
                            phase: "queue".into(),
                            message: if saw_worker {
                                "Waiting for command status to be recorded..."
                            } else {
                                "Waiting for a worker to claim this command..."
                            }
                            .into(),
                        },
                    };
                    if tx.send(CommandLogUpdate::Waiting(true)).await.is_err()
                        || tx.send(CommandLogUpdate::transient(waiting)).await.is_err()
                    {
                        return;
                    }
                    match wait_for_worker(&state, &command_id, &tx, poll).await {
                        WorkerWaitOutcome::ClientGone | WorkerWaitOutcome::Unavailable => return,
                        WorkerWaitOutcome::Terminal(attempt) => {
                            cursor.update_attempt(attempt);
                            finish_terminal_log(
                                &state,
                                &session_id,
                                &command_id,
                                &mut cursor,
                                &mut live_rx,
                                &tx,
                            )
                            .await;
                            return;
                        }
                        WorkerWaitOutcome::Bus(receiver) => {
                            if tx.send(CommandLogUpdate::Waiting(false)).await.is_err() {
                                return;
                            }
                            saw_worker = true;
                            live_rx = Some(receiver);
                            let events = command_log_events(&state, &session_id, &command_id).await;
                            if !cursor.replay(&events, &tx).await {
                                return;
                            }
                        }
                    }
                }

                let Some(receiver) = live_rx.as_mut() else {
                    continue;
                };
                let mut status_poll = tokio::time::interval(poll);
                loop {
                    tokio::select! {
                        _ = tx.closed() => return,
                        _ = status_poll.tick() => {
                            let Some(row) = command_log_row(&state, &command_id).await else {
                                return;
                            };
                            cursor.update_attempt(row.attempt);
                            if row.status.is_terminal() {
                                finish_terminal_log(&state, &session_id, &command_id, &mut cursor, &mut live_rx, &tx).await;
                                return;
                            }
                            // Events published before this subscription, or lost
                            // to lag, reach the client only through the log. Do
                            // not leave them waiting on a later event that a quiet
                            // command may never emit.
                            if command_log_is_behind(&state, &command_id, &cursor).await {
                                let events = command_log_events(&state, &session_id, &command_id).await;
                                if !cursor.replay(&events, &tx).await {
                                    return;
                                }
                            }
                        }
                        result = receiver.recv() => match result {
                            Ok(event) => {
                                cursor.update_attempt(event.attempt);
                                if cursor.missing_before(&event)
                                    && !recover_command_log_gap(
                                        &state, &session_id, &command_id, event.cursor(), &mut cursor, &tx,
                                    ).await
                                {
                                    return;
                                }
                                if !cursor.forward(&event, &tx).await {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(lagged = n, "Command log subscriber lagged");
                                if tx.send(CommandLogUpdate::Lagged(n)).await.is_err() {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                // A vanished bus does not prove completion. Re-enter
                                // worker waiting and check the row before sending Done.
                                // A row already terminal finishes here, so no waiting
                                // frame lands between the output and the final Done.
                                let Some(row) = command_log_row(&state, &command_id).await else {
                                    return;
                                };
                                cursor.update_attempt(row.attempt);
                                live_rx = None;
                                if row.status.is_terminal() {
                                    finish_terminal_log(&state, &session_id, &command_id, &mut cursor, &mut live_rx, &tx).await;
                                    return;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        });
        Self { receiver, task }
    }

    /// Next execution event, including id-less queue/recovery notifications.
    /// Receiving is cancellation-safe: timeouts never discard an event.
    pub async fn next(&mut self) -> Option<ExecutionEvent> {
        loop {
            if let CommandLogUpdate::Event { event, .. } = self.next_update().await? {
                return Some(*event);
            }
        }
    }

    /// Event delivery metadata and progress notices for transport adapters.
    /// No wire encoding or keepalive timer belongs to this stream.
    pub async fn next_update(&mut self) -> Option<CommandLogUpdate> {
        self.receiver.recv().await
    }
}

impl Drop for CommandLogStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type CommandLogSender = tokio::sync::mpsc::Sender<CommandLogUpdate>;
type CommandLogReceiver = tokio::sync::broadcast::Receiver<ExecutionEvent>;

/// Per-subscription replay boundary and the last event delivered. Done is
/// held back until the row or its matching sidecar is terminal.
struct CommandLogCursor {
    replay_from: ReplayFrom,
    last_sent: Option<EventCursor>,
    attempt: u32,
    next_sequence: u64,
    done: Option<ExecutionEvent>,
    terminal_status_seen: bool,
    /// `?step=<name>`: events of other steps are skipped, not sent. The
    /// high-water mark still advances past them, so the client's cursor
    /// stays global (WI 0137 §5).
    step_filter: Option<String>,
}

impl CommandLogCursor {
    fn new(replay_from: ReplayFrom, attempt: u32) -> Self {
        Self {
            replay_from,
            last_sent: match replay_from {
                ReplayFrom::After(cursor) => Some(cursor),
                _ => None,
            },
            attempt,
            next_sequence: 0,
            done: None,
            terminal_status_seen: false,
            step_filter: None,
        }
    }

    fn with_step_filter(mut self, step: Option<String>) -> Self {
        self.step_filter = step.filter(|step| !step.is_empty());
        self
    }

    fn update_attempt(&mut self, attempt: u32) {
        if attempt > self.attempt {
            self.attempt = attempt;
            self.next_sequence = 0;
            self.done = None;
            self.terminal_status_seen = false;
            if matches!(self.replay_from, ReplayFrom::StartOfAttempt(_)) {
                self.replay_from = ReplayFrom::StartOfAttempt(attempt);
            }
        }
    }

    async fn send_frame(&mut self, event: &ExecutionEvent, tx: &CommandLogSender) -> bool {
        if !self.replay_from.includes(event)
            || self.last_sent.is_some_and(|last| event.cursor() <= last)
        {
            return true;
        }
        if let Some(step) = &self.step_filter {
            if !event.matches_step_filter(step) {
                // Skipped, but counted: the cursor stays global.
                self.last_sent = Some(event.cursor());
                return true;
            }
        }
        if tx
            .send(CommandLogUpdate::persisted(event.clone()))
            .await
            .is_err()
        {
            return false;
        }
        self.last_sent = Some(event.cursor());
        true
    }

    async fn forward(&mut self, event: &ExecutionEvent, tx: &CommandLogSender) -> bool {
        if event.attempt == self.attempt {
            if matches!(event.payload, EventPayload::CommandStatus { .. }) {
                self.terminal_status_seen = true;
            }
            self.next_sequence = self.next_sequence.max(event.sequence.saturating_add(1));
            if matches!(event.payload, EventPayload::Done) {
                self.done = Some(event.clone());
            }
        }
        // An earlier attempt may have written Done just before a daemon crash.
        // Neither that marker nor the current marker can end a nonterminal row.
        if matches!(event.payload, EventPayload::Done) {
            return true;
        }
        self.send_frame(event, tx).await
    }

    async fn replay(&mut self, events: &[ExecutionEvent], tx: &CommandLogSender) -> bool {
        // Legacy logs can contain inversions from concurrent producers. Sort
        // before applying the high-water mark so full replay loses no records.
        let mut events: Vec<_> = events.iter().collect();
        events.sort_by_key(|event| event.cursor());
        for event in events {
            if !self.forward(event, tx).await {
                return false;
            }
        }
        true
    }

    fn missing_before(&self, event: &ExecutionEvent) -> bool {
        match self.last_sent {
            Some(last) if last.attempt == event.attempt => {
                event.sequence > last.sequence.saturating_add(1)
            }
            Some(last) if last.attempt > event.attempt => false,
            _ => event.sequence > 0,
        }
    }

    async fn finish(&mut self, tx: &CommandLogSender) {
        // Early failures and queued cancellations can have no events.log.
        let done = self.done.take().unwrap_or_else(|| ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt: self.attempt,
            sequence: self.next_sequence,
            source: None,
            payload: EventPayload::Done,
        });
        // Completion is a terminal-state notification as well as a log event.
        // Even an ahead-of-head resume must learn that the command ended.
        let _ = tx.send(CommandLogUpdate::persisted(done)).await;
    }
}

enum WorkerWaitOutcome {
    Terminal(u32),
    Bus(CommandLogReceiver),
    ClientGone,
    Unavailable,
}

/// SQLite can wait on a busy lock; never run these polls on a Tokio worker.
async fn command_log_row(state: &CommandLogSource, command_id: &str) -> Option<CommandRecord> {
    let store = Arc::clone(&state.store);
    let id = command_id.to_owned();
    let paths = state.paths.clone();
    match tokio::task::spawn_blocking(move || {
        let mut row = store.get_command(&id)?;
        if let Some(row) = &mut row {
            // F01 can durably finish before SQLite accepts the terminal update.
            // Project only matching terminal evidence; never treat a historical
            // Done frame (or another attempt's sidecar) as completion.
            if row.status == crate::data::command_status::CommandStatus::Running {
                if let Some(completion) = paths.read_command_completion(&row.session_id, &id)? {
                    if completion.attempt == row.attempt && completion.status.is_terminal() {
                        row.status = completion.status;
                        row.exit_code = Some(completion.exit_code);
                        row.result = Some(completion.result_json);
                        row.finished_at = Some(completion.finished_at);
                    }
                }
            }
        }
        Ok::<_, crate::data::error::DataError>(row)
    })
    .await
    {
        Ok(Ok(row)) => row,
        Ok(Err(error)) => {
            tracing::error!(%error, %command_id, "Failed to poll command status");
            None
        }
        Err(error) => {
            tracing::error!(%error, %command_id, "Command status poll failed");
            None
        }
    }
}

async fn command_log_events(
    state: &CommandLogSource,
    session_id: &str,
    command_id: &str,
) -> Vec<ExecutionEvent> {
    let paths = state.paths.clone();
    let session = session_id.to_owned();
    let command = command_id.to_owned();
    tokio::task::spawn_blocking(move || paths.read_command_events(&session, &command))
        .await
        .unwrap_or_default()
}

/// A terminal row precedes its final events. Wait for the actual marker so
/// a disk-writer delay cannot mint a cursor that hides the final result.
async fn finish_terminal_log(
    state: &CommandLogSource,
    session_id: &str,
    command_id: &str,
    cursor: &mut CommandLogCursor,
    live_rx: &mut Option<CommandLogReceiver>,
    tx: &CommandLogSender,
) {
    let terminal_row = command_log_row(state, command_id).await;
    let structured = terminal_row
        .as_ref()
        .and_then(|row| row.result.as_deref())
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .is_some_and(|result| result.get("outcome").is_some());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let events = command_log_events(state, session_id, command_id).await;
        if !cursor.replay(&events, tx).await {
            return;
        }
        if cursor.done.is_some() || !structured {
            cursor.finish(tx).await;
            return;
        }
        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep_until(deadline) => {
                tracing::warn!(command_id, "Terminal command log has not caught up");
                // An unwritable command directory can leave only the row.
                // These recovery notifications have no id: they must never
                // consume the eventual persisted pair's cursor namespace.
                if let Some(row) = &terminal_row {
                    let payloads = if cursor.terminal_status_seen {
                        vec![EventPayload::Done]
                    } else {
                        vec![crate::command::commands::api_server::queue_worker::terminal_payload(row), EventPayload::Done]
                    };
                    for payload in payloads {
                        let event = ExecutionEvent { timestamp: chrono::Utc::now(), attempt: row.attempt, sequence: 0, source: None, payload };
                        if tx.send(CommandLogUpdate::transient(event)).await.is_err() { return; }
                    }
                }
                return;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {},
            event = async {
                match live_rx.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => match event {
                Ok(event) => {
                    if cursor.missing_before(&event) && !recover_command_log_gap(
                        state, session_id, command_id, event.cursor(), cursor, tx,
                    ).await { return; }
                    if !cursor.forward(&event, tx).await { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => *live_rx = None,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
            }
        }
    }
}

/// A subscription can start ahead of the independent logfile writer. Hold
/// the live event until the log catches up, replaying the missing prefix before
/// advancing the client's id. The same path repairs broadcast lag. On failure,
/// close without Done so the client resumes from its actual delivered cursor.
/// Whether the running attempt's bus has published past what `cursor` has seen.
async fn command_log_is_behind(
    state: &CommandLogSource,
    command_id: &str,
    cursor: &CommandLogCursor,
) -> bool {
    state
        .event_buses
        .lock()
        .await
        .get(command_id)
        .is_some_and(|bus| {
            bus.attempt() == cursor.attempt && bus.next_sequence() > cursor.next_sequence
        })
}

async fn recover_command_log_gap(
    state: &CommandLogSource,
    session_id: &str,
    command_id: &str,
    through: EventCursor,
    cursor: &mut CommandLogCursor,
    tx: &CommandLogSender,
) -> bool {
    let recovery = async {
        loop {
            let events = command_log_events(state, session_id, command_id).await;
            if events.iter().any(|event| event.cursor() == through) {
                return cursor.replay(&events, tx).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    tokio::select! {
        _ = tx.closed() => false,
        result = tokio::time::timeout(std::time::Duration::from_secs(5), recovery) => {
            match result {
                Ok(sent) => sent,
                Err(_) => {
                    tracing::warn!(%command_id, %through, "Command log gap could not be recovered");
                    false
                }
            }
        }
    }
}

/// Wait indefinitely, polling independently of the comment keepalive cadence.
/// Subscribe before returning; live gap recovery also waits for the independent
/// logfile writer to catch up before advancing the response cursor.
async fn wait_for_worker(
    state: &CommandLogSource,
    command_id: &str,
    tx: &CommandLogSender,
    poll: std::time::Duration,
) -> WorkerWaitOutcome {
    let mut status_poll = tokio::time::interval(poll);
    loop {
        tokio::select! {
            _ = tx.closed() => return WorkerWaitOutcome::ClientGone,
            _ = status_poll.tick() => {
                let Some(row) = command_log_row(state, command_id).await else {
                    return WorkerWaitOutcome::Unavailable;
                };
                if row.status.is_terminal() {
                    return WorkerWaitOutcome::Terminal(row.attempt);
                }
                if let Some(bus) = state.event_buses.lock().await.get(command_id) {
                    return WorkerWaitOutcome::Bus(bus.subscribe());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::command_status::CommandStatus;
    use crate::data::execution_event::{EventSource, EventSourcePhase};
    use crate::data::fs::api_db::NewSessionRow;
    use crate::data::session::SessionKind;
    use crate::data::session_setup_event::SessionSetupStatus;

    #[tokio::test]
    async fn sidecar_projection_requires_running_matching_terminal_evidence() {
        use crate::data::command_status::CommandCompletion;
        let tmp = tempfile::tempdir().unwrap();
        let source = source(tmp.path());
        let row = source.store.claim_next_command("worker").unwrap().unwrap();
        for (attempt, status) in [
            (row.attempt + 1, CommandStatus::Done),
            (row.attempt, CommandStatus::Running),
            (row.attempt, CommandStatus::Done),
        ] {
            source
                .paths
                .write_command_completion(
                    "s",
                    "c",
                    &CommandCompletion {
                        attempt,
                        status,
                        exit_code: 7,
                        result_json: "{}".into(),
                        finished_at: chrono::Utc::now().to_rfc3339(),
                    },
                )
                .unwrap();
            let projected = command_log_row(&source, "c").await.unwrap();
            assert_eq!(
                projected.status,
                if attempt == row.attempt && status.is_terminal() {
                    CommandStatus::Done
                } else {
                    CommandStatus::Running
                }
            );
            assert_eq!(
                source.store.get_command("c").unwrap().unwrap().status,
                CommandStatus::Running
            );
        }
        source
            .store
            .complete_command("c", CommandStatus::Cancelled, Some(130), None)
            .unwrap();
        assert_eq!(
            command_log_row(&source, "c").await.unwrap().status,
            CommandStatus::Cancelled,
            "an already terminal row wins over sidecar evidence"
        );
    }

    fn event(attempt: u32, sequence: u64, payload: EventPayload) -> ExecutionEvent {
        ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt,
            sequence,
            source: None,
            payload,
        }
    }

    fn output(attempt: u32, sequence: u64) -> ExecutionEvent {
        event(
            attempt,
            sequence,
            EventPayload::StdoutLine(format!("line {sequence}")),
        )
    }

    fn source(root: &std::path::Path) -> CommandLogSource {
        let paths = ApiPaths::at_root(root);
        paths.ensure_root().unwrap();
        let store = Arc::new(SqliteSessionStore::open(root).unwrap());
        store
            .insert_session_full(NewSessionRow {
                id: "s",
                workdir: "/work",
                created_at: &chrono::Utc::now().to_rfc3339(),
                setup_status: SessionSetupStatus::Ready,
                kind: SessionKind::Local,
                cloned_path: None,
            })
            .unwrap();
        store
            .enqueue_command("c", "s", "exec prompt", "[]", "/unused")
            .unwrap();
        CommandLogSource {
            store,
            paths,
            event_buses: Default::default(),
        }
    }

    fn write_events(source: &CommandLogSource, events: &[ExecutionEvent]) {
        let path = source.paths.command_events_log_path("s", "c");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines: String = events
            .iter()
            .map(|event| format!("{}\n", serde_json::to_string(event).unwrap()))
            .collect();
        std::fs::write(path, lines).unwrap();
    }

    fn delivered(update: CommandLogUpdate) -> (ExecutionEvent, Option<EventCursor>) {
        match update {
            CommandLogUpdate::Event { event, cursor } => (*event, cursor),
            _ => panic!("expected an event"),
        }
    }

    async fn open(source: &CommandLogSource, requested: Option<EventCursor>) -> CommandLogStream {
        CommandLogStream::open(
            Arc::clone(&source.store),
            source.paths.clone(),
            Arc::clone(&source.event_buses),
            source.store.get_command("c").unwrap().unwrap(),
            requested,
            None,
            Duration::from_millis(10),
        )
        .await
    }

    #[tokio::test]
    async fn replay_sorts_inversions_and_deduplicates_live_overlap_after_resume() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut cursor = CommandLogCursor::new(ReplayFrom::After(EventCursor::new(1, 0)), 1);
        assert!(
            cursor
                .replay(
                    &[output(1, 2), output(1, 0), output(1, 1), output(1, 2)],
                    &tx
                )
                .await
        );
        assert!(cursor.forward(&output(1, 2), &tx).await);
        assert!(cursor.forward(&output(1, 3), &tx).await);
        let mut ids = Vec::new();
        while let Ok(update) = rx.try_recv() {
            ids.push(delivered(update).1.unwrap());
        }
        assert_eq!(
            ids,
            vec![
                EventCursor::new(1, 1),
                EventCursor::new(1, 2),
                EventCursor::new(1, 3)
            ]
        );
    }

    #[tokio::test]
    async fn step_filter_advances_global_cursor_and_retains_completion() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut cursor =
            CommandLogCursor::new(ReplayFrom::Beginning, 1).with_step_filter(Some("alpha".into()));
        let mut alpha = output(1, 0);
        alpha.source = Some(EventSource::step(EventSourcePhase::Main, "alpha", 0));
        let mut beta = output(1, 1);
        beta.source = Some(EventSource::step(EventSourcePhase::Main, "beta", 1));
        assert!(
            cursor
                .replay(&[alpha, beta, event(1, 2, EventPayload::Done)], &tx)
                .await
        );
        assert_eq!(cursor.last_sent, Some(EventCursor::new(1, 1)));
        assert!(!cursor.missing_before(&output(1, 2)));
        cursor.finish(&tx).await;
        assert_eq!(
            delivered(rx.recv().await.unwrap()).1,
            Some(EventCursor::new(1, 0))
        );
        assert_eq!(
            delivered(rx.recv().await.unwrap()).1,
            Some(EventCursor::new(1, 2))
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn retry_clears_old_done_and_only_current_done_finishes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut cursor = CommandLogCursor::new(ReplayFrom::StartOfAttempt(1), 1);
        cursor.forward(&event(1, 7, EventPayload::Done), &tx).await;
        assert!(rx.try_recv().is_err());
        cursor.update_attempt(2);
        assert!(cursor.done.is_none());
        assert!(!cursor.terminal_status_seen);
        assert_eq!(cursor.replay_from, ReplayFrom::StartOfAttempt(2));
        cursor.forward(&output(2, 0), &tx).await;
        cursor.forward(&event(2, 1, EventPayload::Done), &tx).await;
        cursor.finish(&tx).await;
        assert_eq!(
            delivered(rx.recv().await.unwrap()).1,
            Some(EventCursor::new(2, 0))
        );
        assert_eq!(
            delivered(rx.recv().await.unwrap()).1,
            Some(EventCursor::new(2, 1))
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_attempt_restarts_and_ahead_cursor_still_gets_done() {
        let tmp = tempfile::tempdir().unwrap();
        let source = source(tmp.path());
        write_events(&source, &[output(1, 0), event(1, 1, EventPayload::Done)]);
        source
            .store
            .complete_command("c", CommandStatus::Done, Some(0), None)
            .unwrap();
        let mut stream = open(&source, Some(EventCursor::new(99, 5))).await;
        assert!(matches!(
            stream.next_update().await,
            Some(CommandLogUpdate::Restart(1))
        ));
        assert_eq!(
            stream.next().await.unwrap().cursor(),
            EventCursor::new(1, 0)
        );
        assert!(matches!(
            stream.next().await.unwrap().payload,
            EventPayload::Done
        ));
        assert!(stream.next().await.is_none());
        let mut ahead = open(&source, Some(EventCursor::new(1, 999))).await;
        assert!(matches!(
            ahead.next().await.unwrap().payload,
            EventPayload::Done
        ));
        assert!(ahead.next().await.is_none());
    }

    #[tokio::test]
    async fn gap_repair_waits_for_disk_and_replays_prefix_before_live_event() {
        let tmp = tempfile::tempdir().unwrap();
        let source = source(tmp.path());
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut cursor = CommandLogCursor::new(ReplayFrom::Beginning, 1);
        let repair =
            recover_command_log_gap(&source, "s", "c", EventCursor::new(1, 2), &mut cursor, &tx);
        let writer = async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            write_events(&source, &[output(1, 0), output(1, 1), output(1, 2)]);
        };
        let (repaired, ()) = tokio::join!(repair, writer);
        assert!(repaired);
        cursor.forward(&output(1, 2), &tx).await;
        for sequence in 0..3 {
            assert_eq!(
                delivered(rx.recv().await.unwrap()).1,
                Some(EventCursor::new(1, sequence))
            );
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unrecoverable_gap_does_not_advance_or_invent_done() {
        let tmp = tempfile::tempdir().unwrap();
        let source = source(tmp.path());
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut cursor = CommandLogCursor::new(ReplayFrom::Beginning, 1);
        assert!(
            !recover_command_log_gap(&source, "s", "c", EventCursor::new(1, 2), &mut cursor, &tx)
                .await
        );
        assert!(cursor.last_sent.is_none());
        assert!(cursor.done.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn structured_terminal_without_a_log_uses_idless_recovery_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let source = source(tmp.path());
        source
            .store
            .complete_command(
                "c",
                CommandStatus::Error,
                Some(1),
                Some(r#"{"outcome":"error"}"#),
            )
            .unwrap();
        let mut stream = open(&source, None).await;
        let (status, cursor) = delivered(stream.next_update().await.unwrap());
        assert!(matches!(status.payload, EventPayload::CommandStatus { .. }));
        assert!(cursor.is_none());
        let (done, cursor) = delivered(stream.next_update().await.unwrap());
        assert!(matches!(done.payload, EventPayload::Done));
        assert!(cursor.is_none());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_one_stream_releases_only_its_subscription() {
        let tmp = tempfile::tempdir().unwrap();
        let source = source(tmp.path());
        let bus = Arc::new(EventBus::new(16));
        let sender = bus.sender();
        source
            .event_buses
            .lock()
            .await
            .insert("c".into(), Arc::clone(&bus));
        let mut first = open(&source, None).await;
        let mut second = open(&source, None).await;
        assert!(matches!(
            first.next_update().await,
            Some(CommandLogUpdate::Waiting(false))
        ));
        assert!(matches!(
            second.next_update().await,
            Some(CommandLogUpdate::Waiting(false))
        ));
        let task_finished = first.task.abort_handle();
        drop(first);
        tokio::task::yield_now().await;
        assert!(task_finished.is_finished());
        sender.emit(EventPayload::StdoutLine("still live".into()));
        assert!(matches!(
            second.next().await.unwrap().payload,
            EventPayload::StdoutLine(_)
        ));
        assert!(!source
            .store
            .get_command("c")
            .unwrap()
            .unwrap()
            .status
            .is_terminal());
    }

    #[tokio::test]
    async fn cancelled_receive_keeps_next_event_and_queued_wait_has_no_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let source = source(tmp.path());
        let mut stream = open(&source, None).await;
        assert!(matches!(
            stream.next().await.unwrap().payload,
            EventPayload::StatusMessage { .. }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), stream.next())
                .await
                .is_err()
        );
        source
            .store
            .complete_command("c", CommandStatus::Cancelled, Some(130), None)
            .unwrap();
        assert!(matches!(
            stream.next().await.unwrap().payload,
            EventPayload::Done
        ));
        assert!(stream.next().await.is_none());
    }
}
