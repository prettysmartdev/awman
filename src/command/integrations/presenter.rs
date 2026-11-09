//! `HostPresenter`, `PresenterHandle`, `AnnotationWriter` and
//! `PresenterSupervisor` (WI 0142 Part A §4). A presenter claims every frontend
//! whose `Hello.annotations` carry keys in its namespace, answers `UiRequest`s
//! with a `UiOutcome`, receives instance events, and may set or clear
//! annotations in its own namespace only. A presenter panic, error or timeout
//! never affects the instance: the request falls back to `claimed_by_host:
//! false` and exactly one notice is emitted.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.9 and §9 (I7).
//!
//! Deviations from the design text (recorded in `0142-progress.md`):
//! - `InstanceEvent` does not exist until `instance-backend-refactor`, and the
//!   host would otherwise have to be imported upward. Presenters receive the
//!   small [`PresenterEvent`] projection; the instance host maps into it.
//! - Notices leave through a [`NoticeSink`] closure instead of a
//!   `broadcast::Sender<InstanceEvent>`; the host wires it to `InstanceEvent::Notice`.
//! - Annotation writes return `AnnotationError` (the host's `set_annotations`
//!   already does), and `AnnotationWriter::clear` names the key.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use crate::data::annotations::{AnnotationError, Annotations};
use crate::data::frontend_protocol::{UiOutcome, UiRequest};
use crate::data::ids::{FrontendId, InstanceId, TabId};
use crate::data::instance_snapshot::AnnotationTarget;
use crate::data::message::MessageLevel;

/// How long a presenter may take to answer one UI request.
pub const PRESENTER_TIMEOUT: Duration = Duration::from_secs(2);

/// The subset of instance events a presenter observes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresenterEvent {
    TabOpened {
        tab_id: TabId,
    },
    TabClosed {
        tab_id: TabId,
    },
    Notice {
        level: MessageLevel,
        text: String,
    },
    Ended,
    /// A frontend reported a container-window view change (Hidden/Minimized/Maximized).
    ContainerWindow {
        tab_id: TabId,
        slot_id: crate::data::instance_snapshot::SlotId,
        state: crate::data::instance_snapshot::ContainerWindowState,
    },
    /// An agent or host-process terminal of a tab opened.
    TerminalOpened {
        tab_id: TabId,
        terminal_id: crate::data::ids::TerminalId,
    },
    TerminalClosed {
        tab_id: TabId,
        terminal_id: crate::data::ids::TerminalId,
        exit_code: Option<i32>,
    },
    /// A terminal viewer went away, with its subscription's annotations and
    /// the reason (for example `hangup`).
    TerminalViewerDetached {
        terminal_id: crate::data::ids::TerminalId,
        reason: String,
        annotations: Annotations,
    },
    /// A program status change in a slot.
    ProgramStatus {
        tab_id: TabId,
        slot_id: crate::data::instance_snapshot::SlotId,
    },
}

/// Receives the single notice a presenter failure produces.
pub type NoticeSink = Arc<dyn Fn(MessageLevel, String) + Send + Sync>;

/// The host's annotation store as seen by presenters. The namespace is already
/// checked by [`PresenterHandle`]; implementations apply `patch` to the
/// target's annotations.
pub trait AnnotationWriter: Send + Sync {
    fn set(
        &self,
        target: &AnnotationTarget,
        namespace: &str,
        patch: &Annotations,
    ) -> Result<(), AnnotationError>;
    fn clear(
        &self,
        target: &AnnotationTarget,
        namespace: &str,
        key: &str,
    ) -> Result<(), AnnotationError>;
}

/// A presenter's write access: its own namespace, nothing else.
#[derive(Clone)]
pub struct PresenterHandle {
    namespace: &'static str,
    writer: Arc<dyn AnnotationWriter>,
}

impl PresenterHandle {
    pub fn new(namespace: &'static str, writer: Arc<dyn AnnotationWriter>) -> Self {
        Self { namespace, writer }
    }

    pub fn namespace(&self) -> &'static str {
        self.namespace
    }

    fn check(&self, key: &str) -> Result<(), AnnotationError> {
        if Annotations::namespace_of(key) == Some(self.namespace) {
            Ok(())
        } else {
            Err(AnnotationError::ForeignNamespace {
                key: key.into(),
                namespace: self.namespace.into(),
            })
        }
    }

    pub fn set(
        &self,
        target: &AnnotationTarget,
        key: &str,
        value: &str,
    ) -> Result<(), AnnotationError> {
        self.check(key)?;
        let patch = Annotations::try_from_pairs([(key, value)])?;
        self.writer.set(target, self.namespace, &patch)
    }

    pub fn clear(&self, target: &AnnotationTarget, key: &str) -> Result<(), AnnotationError> {
        self.check(key)?;
        self.writer.clear(target, self.namespace, key)
    }
}

/// A Layer 3 adapter for one integration's host (implemented by the integration).
pub trait HostPresenter: Send + Sync {
    fn name(&self) -> &'static str;
    /// Called once, at host start.
    fn bind(&self, handle: PresenterHandle);
    fn on_ui_request(
        &self,
        instance: &InstanceId,
        frontend: &FrontendId,
        req: &UiRequest,
    ) -> UiOutcome;
    fn on_instance_event(&self, instance: &InstanceId, ev: &PresenterEvent);
    /// 0155 presenter hold.
    fn hold(&self) -> bool {
        false
    }
}

fn describe_panic(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into())
}

fn fall_back(presenter: &str, why: &str, notices: &NoticeSink) -> UiOutcome {
    notices(
        MessageLevel::Warning,
        format!("integration `{presenter}` could not handle a UI request ({why}); handled by the engine instead"),
    );
    UiOutcome::unclaimed()
}

fn finish(presenter: &dyn HostPresenter, mut outcome: UiOutcome) -> UiOutcome {
    if outcome.claimed_by_host && outcome.claimed_by.is_none() {
        outcome.claimed_by = Some(presenter.name().into());
    }
    outcome
}

/// One presenter call with `catch_unwind`; a panic yields `unclaimed()` plus
/// one notice. (No timeout here; [`PresenterSupervisor::ui_request`] adds it.)
pub fn call_ui_request(
    presenter: &dyn HostPresenter,
    instance: &InstanceId,
    frontend: &FrontendId,
    req: &UiRequest,
    notices: &NoticeSink,
) -> UiOutcome {
    match catch_unwind(AssertUnwindSafe(|| {
        presenter.on_ui_request(instance, frontend, req)
    })) {
        Ok(outcome) => finish(presenter, outcome),
        Err(p) => fall_back(presenter.name(), &describe_panic(p.as_ref()), notices),
    }
}

/// Runs presenter calls guarded: panic or timeout → `unclaimed()` + one notice.
pub struct PresenterSupervisor {
    presenters: Vec<Arc<dyn HostPresenter>>,
    notices: NoticeSink,
}

impl PresenterSupervisor {
    /// Binds each presenter to a handle for its own namespace.
    pub fn new(
        presenters: Vec<Arc<dyn HostPresenter>>,
        writer: Arc<dyn AnnotationWriter>,
        notices: NoticeSink,
    ) -> Self {
        for p in &presenters {
            let handle = PresenterHandle::new(p.name(), writer.clone());
            if catch_unwind(AssertUnwindSafe(|| p.bind(handle))).is_err() {
                notices(
                    MessageLevel::Warning,
                    format!(
                        "integration `{}` failed to start its host adapter",
                        p.name()
                    ),
                );
            }
        }
        Self {
            presenters,
            notices,
        }
    }

    pub async fn ui_request(
        &self,
        p: &Arc<dyn HostPresenter>,
        instance: &InstanceId,
        frontend: &FrontendId,
        req: &UiRequest,
    ) -> UiOutcome {
        let (p2, i, f, r, n) = (
            p.clone(),
            instance.clone(),
            frontend.clone(),
            req.clone(),
            self.notices.clone(),
        );
        let job = tokio::task::spawn_blocking(move || call_ui_request(p2.as_ref(), &i, &f, &r, &n));
        match tokio::time::timeout(PRESENTER_TIMEOUT, job).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => fall_back(p.name(), "task failed", &self.notices),
            Err(_) => fall_back(p.name(), "timed out", &self.notices),
        }
    }

    /// Fan an event out; a panicking presenter is skipped with one notice.
    pub fn instance_event(&self, instance: &InstanceId, ev: &PresenterEvent) {
        for p in &self.presenters {
            if catch_unwind(AssertUnwindSafe(|| p.on_instance_event(instance, ev))).is_err() {
                (self.notices)(
                    MessageLevel::Warning,
                    format!("integration `{}` failed while observing an event", p.name()),
                );
            }
        }
    }

    pub fn holds(&self) -> usize {
        self.presenters
            .iter()
            .filter(|p| catch_unwind(AssertUnwindSafe(|| p.hold())).unwrap_or(false))
            .count()
    }
}
