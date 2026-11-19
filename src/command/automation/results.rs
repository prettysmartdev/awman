//! The shared Layer 2 result policy (WI 0070 B6/B7).
//!
//! CLI, TUI and API receive the same typed results: no frontend decides
//! success by recognizing a provider record. Ordinary (non-automation)
//! launches keep their candidate fallback and exit semantics; automation
//! launches use the strict policy here.

use crate::data::agent_policy::NativeStartMode;
use crate::data::automation::{CleanupStatus, OutputStatus, StatusSupport, TurnStatus};
use crate::data::automation_records::{
    AutomationFailure, CommandAutomationResult, TurnRecord, TurnResult,
};
use crate::data::ids::ExecutionId;
use crate::data::settings_profile::NativeSessionId;
use crate::data::workspace::WorkspaceAccess;

use super::admission::{codes, failure};

/// The latest execution's facts the aggregate reports.
#[derive(Debug, Clone)]
pub struct ExecutionFacts {
    pub execution_id: ExecutionId,
    pub native_session_id: Option<NativeSessionId>,
    pub native_start_mode: NativeStartMode,
    pub workspace_access: WorkspaceAccess,
    /// The execution's established tier when no turn was accepted.
    pub established: StatusSupport,
}

/// Aggregate a command's turns (all executions, ordinal order) into its
/// automation assurance. `completion_verified` is true only when at least
/// one turn was accepted and every accepted turn is native-confirmed.
pub fn aggregate(
    turns: &[TurnRecord],
    latest: &ExecutionFacts,
    output_status: OutputStatus,
    cleanup_status: CleanupStatus,
) -> CommandAutomationResult {
    let status_support = turns
        .iter()
        .map(|t| {
            t.result
                .as_ref()
                .map_or(StatusSupport::None, |r| r.status_support)
        })
        .reduce(StatusSupport::weakest)
        .unwrap_or(latest.established);
    let completion_verified = !turns.is_empty()
        && turns
            .iter()
            .all(|t| t.result.as_ref().is_some_and(|r| r.completion_verified));
    CommandAutomationResult {
        execution_id: latest.execution_id.clone(),
        native_session_id: latest.native_session_id.clone(),
        last_turn_id: turns.last().map(|t| t.turn_id.clone()),
        native_start_mode: latest.native_start_mode,
        workspace_access: latest.workspace_access,
        status_support,
        completion_verified: completion_verified && output_status == OutputStatus::Complete,
        output_status,
        cleanup_status,
    }
}

/// The command's final message: the last accepted turn's native-confirmed
/// answer, or `None`. Never an earlier turn's answer.
pub fn final_message(turns: &[TurnRecord]) -> Option<String> {
    let last = turns.last()?.result.as_ref()?;
    (last.status == TurnStatus::Completed && last.completion_verified)
        .then(|| last.final_message.clone())
        .flatten()
}

/// Inputs to the final command failure decision.
#[derive(Debug, Clone, Default)]
pub struct FailureInputs {
    /// A primary execution failure (startup, protocol, delivery, wait…).
    pub execution_failure: Option<AutomationFailure>,
    pub cancelled: bool,
    pub output_status: Option<OutputStatus>,
    pub cleanup_status: Option<CleanupStatus>,
}

/// The command's primary failure, by precedence: an established provider
/// turn failure stays primary; accepted cancellation reports cancelled
/// without erasing it; otherwise delivery/protocol/runtime/startup
/// failures precede secondary output/cleanup failures. Incomplete output
/// or cleanup prevents success. A later successful turn never erases an
/// earlier failure; `completed_unverified` alone is not a failure.
pub fn command_failure(turns: &[TurnRecord], inputs: &FailureInputs) -> Option<AutomationFailure> {
    let turn_failure = turns.iter().find_map(|t| {
        let r = t.result.as_ref()?;
        match r.status {
            TurnStatus::Failed | TurnStatus::DeliveryUnknown => {
                Some(r.failure.clone().unwrap_or_else(|| {
                    failure(
                        if r.status == TurnStatus::Failed {
                            codes::AGENT_TURN_FAILED
                        } else {
                            codes::DELIVERY_UNKNOWN
                        },
                        "an accepted turn did not complete",
                    )
                }))
            }
            _ => None,
        }
    });
    if let Some(f) = turn_failure
        .clone()
        .filter(|f| f.code == codes::AGENT_TURN_FAILED)
    {
        return Some(f);
    }
    if inputs.cancelled {
        return Some(failure(codes::CANCELLED, "the command was cancelled"));
    }
    if let Some(f) = inputs.execution_failure.clone() {
        return Some(f);
    }
    if let Some(f) = turn_failure {
        return Some(f);
    }
    if inputs.output_status == Some(OutputStatus::Incomplete) {
        return Some(failure(
            codes::OUTPUT_INCOMPLETE,
            "output was not fully drained before the deadline",
        ));
    }
    match inputs.cleanup_status {
        Some(CleanupStatus::Pending) => Some(failure(
            codes::CLEANUP_TIMEOUT,
            "owned resources were not confirmed removed before the deadline",
        )),
        Some(CleanupStatus::Failed) => Some(failure(
            codes::CLEANUP_TIMEOUT,
            "owned resource cleanup failed",
        )),
        _ => None,
    }
}

/// What a strict headless automation run observed.
#[derive(Debug, Clone, Default)]
pub struct HeadlessObservation {
    pub identity: Option<NativeSessionId>,
    /// A correlated native terminal outcome: `Ok(answer)` or `Err(message)`.
    pub outcome: Option<Result<Option<String>, String>>,
    /// The translator reported a malformed required record.
    pub protocol_error: Option<String>,
}

/// Strict headless result: identity + correlated native outcome + actual
/// exit zero. Exit zero alone, a candidate-only stream or a missing final
/// record never succeed; a known provider failure or nonzero exit stays
/// primary even when secondary drain/cleanup errors occur.
pub fn headless_result(
    observed: &HeadlessObservation,
    exit: Result<i32, String>,
    weakest: StatusSupport,
) -> (TurnResult, Option<AutomationFailure>) {
    use crate::data::automation::{CompletionSource, ReadinessSource};
    let base = |status, source, verified, message, failure: Option<AutomationFailure>| TurnResult {
        status,
        status_support: weakest,
        readiness_source: ReadinessSource::Native,
        completion_source: source,
        completion_verified: verified,
        native_turn_id: None,
        final_message: message,
        output_status: OutputStatus::Complete,
        failure,
    };
    let exit_failure = match &exit {
        Ok(0) => None,
        Ok(code) => Some(failure(
            codes::AGENT_TURN_FAILED,
            &format!("the agent exited with status {code}"),
        )),
        Err(e) => Some(failure(codes::RUNTIME_WAIT_FAILED, e)),
    };
    if let Some(Err(message)) = &observed.outcome {
        let f = failure(codes::AGENT_TURN_FAILED, message);
        return (
            base(
                TurnStatus::Failed,
                CompletionSource::Native,
                true,
                None,
                Some(f.clone()),
            ),
            Some(f),
        );
    }
    if let Some(f) = exit_failure {
        return (
            base(
                TurnStatus::Failed,
                CompletionSource::None,
                false,
                None,
                Some(f.clone()),
            ),
            Some(f),
        );
    }
    if let Some(reason) = &observed.protocol_error {
        let f = failure(codes::AGENT_PROTOCOL_ERROR, reason);
        let mut r = base(
            TurnStatus::Failed,
            CompletionSource::None,
            false,
            None,
            Some(f.clone()),
        );
        r.output_status = OutputStatus::Incomplete;
        return (r, Some(f));
    }
    match (&observed.identity, &observed.outcome) {
        (Some(_), Some(Ok(answer))) => (
            base(
                TurnStatus::Completed,
                CompletionSource::Native,
                true,
                Some(answer.clone().unwrap_or_default()),
                None,
            ),
            None,
        ),
        (None, _) => {
            let f = failure(
                codes::AGENT_PROTOCOL_ERROR,
                "the provider reported no native session identity",
            );
            let mut r = base(
                TurnStatus::Failed,
                CompletionSource::None,
                false,
                None,
                Some(f.clone()),
            );
            r.output_status = OutputStatus::Incomplete;
            (r, Some(f))
        }
        (Some(_), None | Some(Err(_))) => {
            let f = failure(
                codes::AGENT_PROTOCOL_ERROR,
                "the provider stream ended without a native final result",
            );
            let mut r = base(
                TurnStatus::Failed,
                CompletionSource::None,
                false,
                None,
                Some(f.clone()),
            );
            r.output_status = OutputStatus::Incomplete;
            (r, Some(f))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::automation::{CompletionSource, ReadinessSource, StatusSource};
    use crate::data::ids::{CommandId, TurnId};

    fn record(
        status: TurnStatus,
        verified: bool,
        support: StatusSupport,
        answer: Option<&str>,
    ) -> TurnRecord {
        TurnRecord {
            request_id: uuid::Uuid::new_v4(),
            turn_id: TurnId::new(),
            execution_id: ExecutionId::new(),
            command_id: CommandId::new(),
            attempt: 1,
            ordinal: 1,
            native_session_id: None,
            native_turn_id: None,
            accepted_at: chrono::Utc::now(),
            started_at: None,
            completed_at: Some(chrono::Utc::now()),
            accepted_revision: 1,
            status_support: support,
            status_source: StatusSource::Native,
            result: Some(TurnResult {
                status,
                status_support: support,
                readiness_source: ReadinessSource::Native,
                completion_source: if verified {
                    CompletionSource::Native
                } else {
                    CompletionSource::Osc7501
                },
                completion_verified: verified,
                native_turn_id: None,
                final_message: answer.map(str::to_owned),
                output_status: OutputStatus::Complete,
                failure: (status == TurnStatus::Failed)
                    .then(|| failure(codes::AGENT_TURN_FAILED, "x")),
            }),
        }
    }

    fn facts() -> ExecutionFacts {
        ExecutionFacts {
            execution_id: ExecutionId::new(),
            native_session_id: None,
            native_start_mode: NativeStartMode::Default,
            workspace_access: WorkspaceAccess::ReadWrite,
            established: StatusSupport::BestEffort,
        }
    }

    #[test]
    fn verification_requires_every_accepted_turn_native_confirmed() {
        let full = record(TurnStatus::Completed, true, StatusSupport::Full, Some("a"));
        let osc = record(
            TurnStatus::CompletedUnverified,
            false,
            StatusSupport::BestEffort,
            None,
        );
        let a = aggregate(
            std::slice::from_ref(&full),
            &facts(),
            OutputStatus::Complete,
            CleanupStatus::Complete,
        );
        assert!(a.completion_verified);
        assert_eq!(a.status_support, StatusSupport::Full);
        let a = aggregate(
            &[full.clone(), osc.clone()],
            &facts(),
            OutputStatus::Complete,
            CleanupStatus::Complete,
        );
        assert!(!a.completion_verified);
        assert_eq!(a.status_support, StatusSupport::BestEffort);
        let a = aggregate(
            &[],
            &facts(),
            OutputStatus::Complete,
            CleanupStatus::Complete,
        );
        assert!(!a.completion_verified);
        assert_eq!(a.status_support, StatusSupport::BestEffort);
        // Unverified-only closes without failure.
        assert_eq!(command_failure(&[osc], &FailureInputs::default()), None);
    }

    #[test]
    fn final_message_is_only_the_last_turns_confirmed_answer() {
        let first = record(
            TurnStatus::Completed,
            true,
            StatusSupport::Full,
            Some("old"),
        );
        let second = record(
            TurnStatus::CompletedUnverified,
            false,
            StatusSupport::BestEffort,
            None,
        );
        assert_eq!(final_message(&[first.clone(), second]), None);
        let empty = record(TurnStatus::Completed, true, StatusSupport::Full, Some(""));
        assert_eq!(final_message(&[first, empty]), Some(String::new()));
    }

    #[test]
    fn failure_precedence() {
        let failed = record(TurnStatus::Failed, true, StatusSupport::Full, None);
        let ok = record(TurnStatus::Completed, true, StatusSupport::Full, Some("a"));
        let inputs = FailureInputs {
            cancelled: true,
            output_status: Some(OutputStatus::Incomplete),
            ..Default::default()
        };
        // A provider failure stays primary over cancellation and a later success.
        assert_eq!(
            command_failure(&[failed, ok.clone()], &inputs)
                .unwrap()
                .code,
            codes::AGENT_TURN_FAILED
        );
        assert_eq!(
            command_failure(std::slice::from_ref(&ok), &inputs)
                .unwrap()
                .code,
            codes::CANCELLED
        );
        let inputs = FailureInputs {
            output_status: Some(OutputStatus::Incomplete),
            cleanup_status: Some(CleanupStatus::Pending),
            ..Default::default()
        };
        assert_eq!(
            command_failure(std::slice::from_ref(&ok), &inputs)
                .unwrap()
                .code,
            codes::OUTPUT_INCOMPLETE
        );
        let inputs = FailureInputs {
            cleanup_status: Some(CleanupStatus::Pending),
            ..Default::default()
        };
        assert_eq!(
            command_failure(&[ok], &inputs).unwrap().code,
            codes::CLEANUP_TIMEOUT
        );
    }

    #[test]
    fn strict_headless_needs_identity_outcome_and_exit_zero() {
        let id = NativeSessionId::parse("s").unwrap();
        let ok = HeadlessObservation {
            identity: Some(id.clone()),
            outcome: Some(Ok(Some(String::new()))),
            protocol_error: None,
        };
        let (r, f) = headless_result(&ok, Ok(0), StatusSupport::Full);
        assert_eq!(r.status, TurnStatus::Completed);
        assert_eq!(r.final_message.as_deref(), Some(""));
        assert!(f.is_none());
        let (_, f) = headless_result(&ok, Ok(3), StatusSupport::Full);
        assert_eq!(f.unwrap().code, codes::AGENT_TURN_FAILED);
        let (_, f) = headless_result(&ok, Err("wait".into()), StatusSupport::Full);
        assert_eq!(f.unwrap().code, codes::RUNTIME_WAIT_FAILED);
        let missing = HeadlessObservation {
            identity: None,
            ..ok.clone()
        };
        assert_eq!(
            headless_result(&missing, Ok(0), StatusSupport::Full)
                .1
                .unwrap()
                .code,
            codes::AGENT_PROTOCOL_ERROR
        );
        let candidate_only = HeadlessObservation {
            identity: Some(id.clone()),
            outcome: None,
            protocol_error: None,
        };
        let (r, f) = headless_result(&candidate_only, Ok(0), StatusSupport::Full);
        assert_eq!(f.unwrap().code, codes::AGENT_PROTOCOL_ERROR);
        assert_eq!(r.output_status, OutputStatus::Incomplete);
        let provider_failed = HeadlessObservation {
            identity: Some(id),
            outcome: Some(Err("quota".into())),
            protocol_error: None,
        };
        let (r, f) = headless_result(&provider_failed, Err("drain".into()), StatusSupport::Full);
        assert!(r.completion_verified);
        assert_eq!(f.unwrap().message, "quota");
    }
}
