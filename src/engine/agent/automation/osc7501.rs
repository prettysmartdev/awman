//! OSC 7501 lifecycle reducer — the explicit best-effort source (WI 0070).
//!
//! Consumes every decoded scanner event in receive order (not the debounced
//! display aggregate) and emits typed observations bound to the execution,
//! the stream generation and a monotone receive sequence. Only id-absent
//! reports are root evidence; `app`, `msg`, `title` and `progress` are never
//! lifecycle or identity evidence. Descendants can only veto root readiness.

use std::collections::BTreeSet;

use crate::data::automation_observation::{
    BlockedReason, Observation, ObservationBinding, ObservationKind, ObservationSource,
};
use crate::data::ids::ExecutionId;
use crate::data::program_status::{
    BlockedKind, ParsedOsc7501, ProgramState, ProgramStatusReport, ReportState,
};
use crate::engine::agent_runtime::program_status::ScanEvent;

/// The display set's record bound. A descendant veto that would be evicted
/// is overflow: assurance is invalidated rather than silently dropped.
pub const MAX_DESCENDANT_VETOES: usize = 64;

/// One execution's OSC lifecycle stream.
#[derive(Debug)]
pub struct OscLifecycle {
    execution_id: ExecutionId,
    enabled: bool,
    generation: u64,
    sequence: u64,
    /// Descendant ids currently working/blocked/error.
    vetoes: BTreeSet<String>,
    overflowed: bool,
    ended: bool,
}

impl OscLifecycle {
    /// `enabled` is the resolved `agents.programStatus.enabled`; when false
    /// nothing is ever emitted (OSC fallback is disabled, not re-enabled).
    pub fn new(execution_id: ExecutionId, enabled: bool) -> Self {
        Self {
            execution_id,
            enabled,
            generation: 1,
            sequence: 0,
            vetoes: BTreeSet::new(),
            overflowed: false,
            ended: false,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn emit(&mut self, kind: ObservationKind) -> Observation {
        self.sequence = crate::data::automation_observation::next_receive_sequence();
        Observation {
            binding: ObservationBinding {
                execution_id: self.execution_id.clone(),
                source: ObservationSource::Osc7501,
                generation: self.generation,
                sequence: self.sequence,
            },
            kind,
        }
    }

    /// Fold one scanner event in, returning the observations it produced.
    pub fn on_scan_event(&mut self, event: &ScanEvent) -> Vec<Observation> {
        if !self.enabled || self.ended {
            return Vec::new();
        }
        match event {
            ScanEvent::Reset => self.reset("terminal reset"),
            ScanEvent::Osc(ParsedOsc7501::Report(report)) => self.report(report),
            // A query and its fixed reply carry no lifecycle evidence.
            ScanEvent::Osc(ParsedOsc7501::Query | ParsedOsc7501::Ignored) => Vec::new(),
        }
    }

    /// The stream ended: invalidate and stop accepting reports.
    pub fn on_stream_end(&mut self) -> Vec<Observation> {
        if !self.enabled || self.ended {
            return Vec::new();
        }
        self.ended = true;
        vec![self.emit(ObservationKind::OscInvalidated {
            reason: "terminal stream ended".into(),
        })]
    }

    fn reset(&mut self, reason: &str) -> Vec<Observation> {
        let had_veto = !self.vetoes.is_empty();
        self.vetoes.clear();
        self.overflowed = false;
        let mut out = vec![self.emit(ObservationKind::OscInvalidated {
            reason: reason.into(),
        })];
        // A new generation: no report of the old one can complete a turn
        // begun under the lost inference.
        self.generation += 1;
        if had_veto {
            out.push(self.emit(ObservationKind::OscDescendantVeto(false)));
        }
        out
    }

    fn report(&mut self, report: &ProgramStatusReport) -> Vec<Observation> {
        if report
            .id
            .as_deref()
            .is_some_and(|id| !crate::data::program_status::is_valid_id(id))
        {
            return Vec::new();
        }
        match &report.id {
            None => self.root(report),
            Some(id) => self.descendant(id, report.state),
        }
    }

    fn root(&mut self, report: &ProgramStatusReport) -> Vec<Observation> {
        let kind = match report.state {
            ReportState::Clear => return self.reset("root status cleared"),
            ReportState::State(ProgramState::Idle) => ObservationKind::OscIdle,
            ReportState::State(ProgramState::Working) => ObservationKind::OscWorking,
            ReportState::State(ProgramState::Done) => ObservationKind::OscDone,
            ReportState::State(ProgramState::Error) => ObservationKind::OscError,
            ReportState::State(ProgramState::Blocked) => {
                ObservationKind::Blocked(match report.kind {
                    Some(BlockedKind::Auth) => BlockedReason::Auth,
                    Some(BlockedKind::Permission) => BlockedReason::Permission,
                    Some(BlockedKind::Question) | None => BlockedReason::Input,
                })
            }
        };
        vec![self.emit(kind)]
    }

    fn descendant(&mut self, id: &str, state: ReportState) -> Vec<Observation> {
        let before = !self.vetoes.is_empty();
        let vetoing = matches!(
            state,
            ReportState::State(ProgramState::Working | ProgramState::Blocked | ProgramState::Error)
        );
        if vetoing {
            if !self.vetoes.contains(id) && self.vetoes.len() >= MAX_DESCENDANT_VETOES {
                // Never evict a veto: invalidate until resynchronization.
                self.overflowed = true;
                return vec![self.emit(ObservationKind::OscInvalidated {
                    reason: "descendant status records overflowed".into(),
                })];
            }
            self.vetoes.insert(id.to_owned());
        } else if matches!(state, ReportState::Clear) {
            // Clearing a subtree removes the record and its descendants.
            let prefix = format!("{id}/");
            self.vetoes
                .retain(|v| v != id && !v.starts_with(prefix.as_str()));
        } else {
            self.vetoes.remove(id);
        }
        let after = !self.vetoes.is_empty() || self.overflowed;
        if before == after {
            return Vec::new();
        }
        vec![self.emit(ObservationKind::OscDescendantVeto(after))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::program_status::Osc7501Scanner;

    fn feed(lifecycle: &mut OscLifecycle, bytes: &[u8]) -> Vec<ObservationKind> {
        let mut scanner = Osc7501Scanner::new();
        scanner
            .feed(bytes)
            .events
            .iter()
            .flat_map(|e| lifecycle.on_scan_event(e))
            .map(|o| o.kind)
            .collect()
    }

    fn osc(body: &str) -> Vec<u8> {
        format!("\x1b]7501;{body}\x07").into_bytes()
    }

    #[test]
    fn root_reports_map_and_metadata_is_not_evidence() {
        let mut l = OscLifecycle::new(ExecutionId::new(), true);
        let mut bytes = osc("state=idle:app=claude:msg=aGk=");
        bytes.extend(osc("state=working:progress=40"));
        bytes.extend(osc("state=done"));
        bytes.extend(osc("state=blocked:kind=auth"));
        bytes.extend(osc("state=blocked"));
        bytes.extend(osc("state=error"));
        assert_eq!(
            feed(&mut l, &bytes),
            vec![
                ObservationKind::OscIdle,
                ObservationKind::OscWorking,
                ObservationKind::OscDone,
                ObservationKind::Blocked(BlockedReason::Auth),
                ObservationKind::Blocked(BlockedReason::Input),
                ObservationKind::OscError,
            ]
        );
    }

    #[test]
    fn descendants_only_veto_and_never_become_root() {
        let mut l = OscLifecycle::new(ExecutionId::new(), true);
        let out = feed(&mut l, &osc("id=sub/1:state=idle"));
        assert!(out.is_empty(), "descendant idle is not root idle: {out:?}");
        assert_eq!(
            feed(&mut l, &osc("id=sub/1:state=working")),
            vec![ObservationKind::OscDescendantVeto(true)]
        );
        assert_eq!(
            feed(&mut l, &osc("id=sub/1:state=done")),
            vec![ObservationKind::OscDescendantVeto(false)]
        );
    }

    #[test]
    fn overflow_invalidates_instead_of_evicting_a_veto() {
        let mut l = OscLifecycle::new(ExecutionId::new(), true);
        for i in 0..MAX_DESCENDANT_VETOES {
            feed(&mut l, &osc(&format!("id=d{i}:state=working")));
        }
        let out = feed(&mut l, &osc("id=extra:state=working"));
        assert!(matches!(out[..], [ObservationKind::OscInvalidated { .. }]));
    }

    #[test]
    fn reset_and_clear_invalidate_and_start_a_new_generation() {
        let mut l = OscLifecycle::new(ExecutionId::new(), true);
        let g = l.generation();
        let out = feed(&mut l, b"\x1bc");
        assert!(matches!(out[..], [ObservationKind::OscInvalidated { .. }]));
        assert_eq!(l.generation(), g + 1);
        let out = feed(&mut l, &osc("state=clear"));
        assert!(matches!(out[..], [ObservationKind::OscInvalidated { .. }]));
        assert!(matches!(
            l.on_stream_end()[..],
            [Observation {
                kind: ObservationKind::OscInvalidated { .. },
                ..
            }]
        ));
        assert!(feed(&mut l, &osc("state=idle")).is_empty());
    }

    #[test]
    fn disabled_reporting_emits_nothing_and_malformed_is_ignored() {
        let mut l = OscLifecycle::new(ExecutionId::new(), false);
        assert!(feed(&mut l, &osc("state=idle")).is_empty());
        let mut l = OscLifecycle::new(ExecutionId::new(), true);
        assert!(feed(&mut l, &osc("state=sleeping")).is_empty());
        assert!(feed(&mut l, &osc("id=../x:state=idle")).is_empty());
        assert!(feed(&mut l, b"\x1b]7501;?\x07").is_empty());
    }

    #[test]
    fn split_sequences_keep_receive_order_and_sequence_is_monotone() {
        let mut l = OscLifecycle::new(ExecutionId::new(), true);
        let mut scanner = Osc7501Scanner::new();
        let bytes = [osc("state=working"), osc("state=done"), osc("state=idle")].concat();
        let mut seen = Vec::new();
        for chunk in bytes.chunks(3) {
            for e in scanner.feed(chunk).events {
                seen.extend(l.on_scan_event(&e));
            }
        }
        let kinds: Vec<_> = seen.iter().map(|o| o.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                ObservationKind::OscWorking,
                ObservationKind::OscDone,
                ObservationKind::OscIdle
            ]
        );
        assert!(seen
            .windows(2)
            .all(|w| w[0].binding.sequence < w[1].binding.sequence));
    }
}
