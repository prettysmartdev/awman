//! Frontend admission for hosted instances (WI 0142 Part A §2, Part B §1):
//! exactly one primary per instance, any number of secondaries (one tab or
//! all), the protocol handshake, and the draining refusal. Pure: the host
//! supplies the facts, admission decides. Layer 3 maps each refusal to its
//! HTTP status and `{error, code}` (see [`AttachRefusal::code`]).

use std::time::Instant;

use crate::data::annotations::AnnotationError;
use crate::data::frontend_protocol::{FrontendRole, Hello, ViewScope, FRONTEND_SUBPROTOCOL};
use crate::data::ids::{FrontendId, TabId};

use super::lifecycle::{FrontendLifecycle, LifecycleRefusal};

/// Why a frontend may not attach.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachRefusal {
    /// 404 `not_found`.
    #[error("instance not found")]
    NotFound,
    /// 409 `primary_exists`.
    #[error("instance already has a primary frontend")]
    PrimaryExists,
    /// 426 `protocol_mismatch`.
    #[error("frontend speaks {offered}; daemon speaks {wanted}")]
    ProtocolMismatch { wanted: String, offered: String },
    /// 503 `feature_disabled`.
    #[error("engine is draining")]
    Draining,
    /// 400 `invalid_annotation`.
    #[error(transparent)]
    InvalidAnnotation(#[from] AnnotationError),
    /// 404 `not_found`.
    #[error("no such tab for a secondary view")]
    UnknownTab,
    /// 503 `instance_starting`: the instance could not be opened.
    #[error("instance failed to start: {0}")]
    StartFailed(String),
}

impl AttachRefusal {
    /// The stable error code of the `{error, code}` body.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound | Self::UnknownTab => "not_found",
            Self::PrimaryExists => "primary_exists",
            Self::ProtocolMismatch { .. } => "protocol_mismatch",
            Self::Draining => "feature_disabled",
            Self::InvalidAnnotation(_) => "invalid_annotation",
            Self::StartFailed(_) => "instance_start_failed",
        }
    }
}

impl From<LifecycleRefusal> for AttachRefusal {
    fn from(refusal: LifecycleRefusal) -> Self {
        match refusal {
            LifecycleRefusal::PrimaryExists => Self::PrimaryExists,
            LifecycleRefusal::NotStarted | LifecycleRefusal::Ended => Self::NotFound,
        }
    }
}

/// What admission granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admitted {
    /// The first primary: the host opens the instance from this `Hello`.
    StartingPrimary {
        frontend_id: FrontendId,
    },
    /// The primary back within its grace, under its old id.
    ResumedPrimary {
        frontend_id: FrontendId,
    },
    Secondary {
        frontend_id: FrontendId,
    },
}

impl Admitted {
    pub fn frontend_id(&self) -> &FrontendId {
        match self {
            Self::StartingPrimary { frontend_id }
            | Self::ResumedPrimary { frontend_id }
            | Self::Secondary { frontend_id } => frontend_id,
        }
    }

    pub fn role(&self) -> FrontendRole {
        match self {
            Self::Secondary { .. } => FrontendRole::Secondary,
            _ => FrontendRole::Primary,
        }
    }
}

/// Decide one `Hello` against an instance's lifecycle, mutating it on
/// success. `tab_exists` answers whether a secondary's `view` tab is open.
pub fn admit(
    hello: &Hello,
    draining: bool,
    lifecycle: &mut FrontendLifecycle,
    tab_exists: impl Fn(&TabId) -> bool,
    now: Instant,
) -> Result<Admitted, AttachRefusal> {
    if hello.protocol != FRONTEND_SUBPROTOCOL {
        return Err(AttachRefusal::ProtocolMismatch {
            wanted: FRONTEND_SUBPROTOCOL.to_string(),
            offered: hello.protocol.clone(),
        });
    }
    if draining {
        return Err(AttachRefusal::Draining);
    }
    match hello.role {
        FrontendRole::Primary => match &hello.resume {
            Some(id) => {
                lifecycle.resume_primary(id, now)?;
                Ok(Admitted::ResumedPrimary {
                    frontend_id: id.clone(),
                })
            }
            None => {
                let frontend_id = FrontendId::new();
                lifecycle.attach_primary(frontend_id.clone())?;
                Ok(Admitted::StartingPrimary { frontend_id })
            }
        },
        FrontendRole::Secondary => {
            if let Some(ViewScope::Tab(tab_id)) = &hello.view {
                if !lifecycle.started() {
                    return Err(AttachRefusal::NotFound);
                }
                if !tab_exists(tab_id) {
                    return Err(AttachRefusal::UnknownTab);
                }
            }
            let frontend_id = FrontendId::new();
            lifecycle.attach_secondary(frontend_id.clone())?;
            Ok(Admitted::Secondary { frontend_id })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::annotations::Annotations;
    use crate::data::frontend_protocol::{FrontendKindWire, HelloEnv, TermSize};

    pub(crate) fn hello(role: FrontendRole) -> Hello {
        Hello {
            protocol: FRONTEND_SUBPROTOCOL.into(),
            build: "test".into(),
            kind: FrontendKindWire::Tui,
            role,
            annotations: Annotations::new(),
            cwd: "/".into(),
            argv: vec!["awman".into()],
            env: HelloEnv::default(),
            term_size: TermSize { cols: 80, rows: 24 },
            stdin_is_tty: true,
            stdout_is_tty: true,
            view: None,
            resume: None,
        }
    }

    #[test]
    fn exactly_one_primary_and_any_number_of_secondaries() {
        let now = Instant::now();
        let mut lifecycle = FrontendLifecycle::reserved(now);
        let first = admit(
            &hello(FrontendRole::Primary),
            false,
            &mut lifecycle,
            |_| true,
            now,
        );
        assert!(matches!(first, Ok(Admitted::StartingPrimary { .. })));
        assert_eq!(
            admit(
                &hello(FrontendRole::Primary),
                false,
                &mut lifecycle,
                |_| true,
                now
            ),
            Err(AttachRefusal::PrimaryExists)
        );
        assert_eq!(AttachRefusal::PrimaryExists.code(), "primary_exists");
        for _ in 0..3 {
            let secondary = admit(
                &hello(FrontendRole::Secondary),
                false,
                &mut lifecycle,
                |_| true,
                now,
            );
            assert_eq!(secondary.map(|a| a.role()), Ok(FrontendRole::Secondary));
        }
        assert_eq!(lifecycle.connected_frontends(), 4);
    }

    #[test]
    fn a_secondary_view_needs_an_open_tab_and_a_started_instance() {
        let now = Instant::now();
        let mut lifecycle = FrontendLifecycle::reserved(now);
        let mut view = hello(FrontendRole::Secondary);
        view.view = Some(ViewScope::Tab(TabId::new()));
        assert_eq!(
            admit(&view, false, &mut lifecycle, |_| true, now),
            Err(AttachRefusal::NotFound)
        );
        admit(
            &hello(FrontendRole::Primary),
            false,
            &mut lifecycle,
            |_| true,
            now,
        )
        .unwrap();
        assert_eq!(
            admit(&view, false, &mut lifecycle, |_| false, now),
            Err(AttachRefusal::UnknownTab)
        );
        assert!(admit(&view, false, &mut lifecycle, |_| true, now).is_ok());
    }

    #[test]
    fn protocol_mismatch_and_draining_are_refused_before_anything_changes() {
        let now = Instant::now();
        let mut lifecycle = FrontendLifecycle::reserved(now);
        let mut old = hello(FrontendRole::Primary);
        old.protocol = "awman.frontend.v0".into();
        assert!(matches!(
            admit(&old, false, &mut lifecycle, |_| true, now),
            Err(AttachRefusal::ProtocolMismatch { .. })
        ));
        assert_eq!(
            admit(
                &hello(FrontendRole::Primary),
                true,
                &mut lifecycle,
                |_| true,
                now
            ),
            Err(AttachRefusal::Draining)
        );
        assert!(!lifecycle.started());
    }

    #[test]
    fn a_resume_takes_back_the_primary_within_its_grace() {
        let now = Instant::now();
        let mut lifecycle = FrontendLifecycle::reserved(now);
        let first = admit(
            &hello(FrontendRole::Primary),
            false,
            &mut lifecycle,
            |_| true,
            now,
        )
        .unwrap();
        let id = first.frontend_id().clone();
        lifecycle.disconnected(&id, now);
        let mut back = hello(FrontendRole::Primary);
        back.resume = Some(id.clone());
        assert_eq!(
            admit(&back, false, &mut lifecycle, |_| true, now),
            Ok(Admitted::ResumedPrimary { frontend_id: id })
        );
    }
}
