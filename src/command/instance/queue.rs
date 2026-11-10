//! An instance owns its manual worker; frontends only request operations.
use super::EngineInstance;
use crate::command::worker::queue_source::{QueueSource, SharedQueueSource};
use crate::data::{session::Session, worker::queue::*};

impl EngineInstance {
    /// A lease already claimed by this instance may provide a prepared
    /// workspace outside a browser launch context. This reads only the
    /// instance's existing worker; it never starts one for a path check.
    pub fn prepared_lease_run(&self, lease_id: &str) -> Option<PickRun> {
        self.queue
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|source| source.prepared_run(lease_id))
    }

    #[cfg(test)]
    pub(crate) fn install_queue_for_test(&self, source: SharedQueueSource) {
        *self.queue.lock().unwrap() = Some(source);
    }
    pub fn queue_source(&self, session: &Session) -> Result<Option<SharedQueueSource>, String> {
        if !session.effective_config().tui_worker_enabled() {
            return Ok(None);
        }
        let mut queue = self.queue.lock().unwrap();
        if queue.is_none() {
            *queue = crate::command::worker::tui_host::TuiWorkerController::start(
                session,
                &self.engines,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(queue.clone())
    }

    pub(super) fn queue_request(&self, session: &Session, request: QueueRequest) -> QueueReply {
        match self.queue_source(session) {
            Ok(Some(source)) => apply(source.as_ref(), request),
            Ok(None) => {
                QueueReply::Error(PickError::Failed("the instance worker is disabled".into()))
            }
            Err(error) => QueueReply::Error(PickError::Failed(error)),
        }
    }
}
fn apply(source: &dyn QueueSource, request: QueueRequest) -> QueueReply {
    match request {
        QueueRequest::Snapshot => QueueReply::Snapshot(source.snapshot()),
        QueueRequest::Refresh => {
            source.refresh();
            QueueReply::Done
        }
        QueueRequest::Seen => {
            source.mark_seen();
            QueueReply::Done
        }
        QueueRequest::Pick { server, item_id } => source
            .pick(&server, &item_id)
            .map(QueueReply::Picked)
            .unwrap_or_else(QueueReply::Error),
        QueueRequest::CheckoutOffer { lease_id } => {
            QueueReply::CheckoutOffer(source.checkout_offer(&lease_id).is_some())
        }
        QueueRequest::Checkout { lease_id, choice } => source
            .choose_checkout(&lease_id, choice)
            .map(QueueReply::Picked)
            .unwrap_or_else(QueueReply::Error),
        QueueRequest::Finish { lease_id, choice } => source
            .finish_lease(&lease_id, choice)
            .map(|()| QueueReply::Done)
            .unwrap_or_else(|e| QueueReply::Error(PickError::Failed(e))),
    }
}
