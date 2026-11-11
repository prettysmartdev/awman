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
    #[error("{0}")]
    ProtocolError(String),
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
            Self::ProtocolError(_) => "protocol_error",
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
    use crate::data::frontend_protocol::{FrontendKindWire, WEB_PRIMARY_RECONNECT_GRACE};
    if hello.kind == FrontendKindWire::Web {
        if !hello.env.is_empty() || !hello.argv.is_empty() || !hello.cwd.as_os_str().is_empty() {
            return Err(AttachRefusal::ProtocolError(
                "web frontends carry no env/cwd/argv".into(),
            ));
        }
        if hello.role == FrontendRole::Primary
            && hello.resume.is_none()
            && hello.launch_context.is_none()
        {
            return Err(AttachRefusal::ProtocolError(
                "web primary requires a launch_context".into(),
            ));
        }
    } else if hello.launch_context.is_some() {
        return Err(AttachRefusal::ProtocolError(
            "only web frontends carry launch_context".into(),
        ));
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
                if hello.kind == FrontendKindWire::Web {
                    lifecycle.set_reconnect_grace(WEB_PRIMARY_RECONNECT_GRACE);
                }
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
