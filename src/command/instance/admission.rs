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
    /// The browser grant the frontend attaches under ended before admission
    /// completed (WI 0160). The socket closes with 1008 and the reason.
    #[error("browser authorization ended: {}", .0.close_reason())]
    GrantEnded(crate::data::browser_auth::GrantRevocationReason),
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
            Self::GrantEnded(_) => "browser_grant_invalid",
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

/// WI 0160 C5: may this `Hello` resume a primary admitted under
/// `recorded`? A primary `resume` must present the same authority (for a
/// browser, the same grant ID; owner and API-key frontends carry none).
/// Anything else is refused exactly like any other second primary. That the
/// presented grant is still **live** is checked by the host, serialized with
/// revocation, around this check and the lifecycle mutation.
pub fn check_resume_authority<A: PartialEq + ?Sized>(
    hello: &Hello,
    recorded: Option<&A>,
    presented: Option<&A>,
) -> Result<(), AttachRefusal> {
    if hello.role == FrontendRole::Primary && hello.resume.is_some() && recorded != presented {
        return Err(AttachRefusal::PrimaryExists);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::annotations::Annotations;
    use crate::data::frontend_protocol::{FrontendKindWire, TermSize, WEB_PRIMARY_RECONNECT_GRACE};
    use crate::data::ids::LaunchContextId;
    use std::time::Duration;

    fn web_hello(resume: Option<FrontendId>) -> Hello {
        Hello {
            protocol: FRONTEND_SUBPROTOCOL.to_string(),
            build: "test".into(),
            kind: FrontendKindWire::Web,
            role: FrontendRole::Primary,
            annotations: Annotations::new(),
            cwd: Default::default(),
            argv: vec![],
            env: Default::default(),
            launch_context: resume.is_none().then(LaunchContextId::new),
            term_size: TermSize { cols: 80, rows: 24 },
            stdin_is_tty: false,
            stdout_is_tty: false,
            view: None,
            resume,
        }
    }

    /// The host's admission sequence: authority check, then lifecycle.
    fn attach(
        lifecycle: &mut FrontendLifecycle,
        hello: &Hello,
        recorded: Option<&str>,
        presented: Option<&str>,
        now: Instant,
    ) -> Result<Admitted, AttachRefusal> {
        check_resume_authority(hello, recorded, presented)?;
        admit(hello, false, lifecycle, |_| true, now)
    }

    #[test]
    fn only_the_admitting_grant_may_resume_a_web_primary_inside_its_grace() {
        let start = Instant::now();
        let mut lifecycle = FrontendLifecycle::reserved(start);
        let first = attach(
            &mut lifecycle,
            &web_hello(None),
            None,
            Some("grant-a"),
            start,
        )
        .unwrap();
        let Admitted::StartingPrimary { frontend_id } = first else {
            panic!("expected a starting primary")
        };
        let recorded = Some("grant-a");
        // A connected primary is never displaced, even by its own grant.
        let resume = web_hello(Some(frontend_id.clone()));
        assert_eq!(
            attach(&mut lifecycle, &resume, recorded, recorded, start),
            Err(AttachRefusal::PrimaryExists)
        );
        let _ = lifecycle.disconnected(&frontend_id, start);
        let inside = start + Duration::from_secs(1);
        // Another grant (a newly re-linked browser) that knows the ID, and a
        // non-browser caller, are refused like any second primary.
        for presented in [Some("grant-b"), None] {
            assert_eq!(
                attach(&mut lifecycle, &resume, recorded, presented, inside),
                Err(AttachRefusal::PrimaryExists)
            );
        }
        // The same grant inside the grace resumes; only one winner.
        assert!(matches!(
            attach(&mut lifecycle, &resume, recorded, recorded, inside),
            Ok(Admitted::ResumedPrimary { .. })
        ));
        assert_eq!(
            attach(&mut lifecycle, &resume, recorded, recorded, inside),
            Err(AttachRefusal::PrimaryExists)
        );
        // Outside the grace even the same grant cannot resume.
        let _ = lifecycle.disconnected(&frontend_id, inside);
        let late = inside + WEB_PRIMARY_RECONNECT_GRACE + Duration::from_secs(1);
        assert!(attach(&mut lifecycle, &resume, recorded, recorded, late).is_err());
    }

    #[test]
    fn non_resume_hellos_and_owner_frontends_are_unaffected() {
        let hello = web_hello(None);
        assert!(check_resume_authority(&hello, Some("grant-a"), Some("grant-b")).is_ok());
        let mut tui = web_hello(Some(FrontendId::new()));
        tui.kind = FrontendKindWire::Tui;
        assert!(check_resume_authority::<str>(&tui, None, None).is_ok());
        let mut secondary = web_hello(None);
        secondary.role = FrontendRole::Secondary;
        assert!(check_resume_authority(&secondary, Some("grant-a"), Some("grant-b")).is_ok());
    }
}
