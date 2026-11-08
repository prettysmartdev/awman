//! What the TUI tells its own terminal over OSC 7501 (WI 0141 hygiene §2).
//!
//! Pure command/presentation policy over Layer 0 status types: given every
//! tab and its agent slots, produce the records to emit. Layer 3
//! (`frontend::osc7501`) encodes them and the TUI writes them; Layer 1 never
//! decides a tab's displayed status or writes to the host terminal.

use crate::data::program_status::{
    strip_invisible, BlockedKind, EffectiveStatus, ProgramState, ProgramStatusReport, ReportState,
    MAX_ID_BYTES, MAX_MSG_BYTES, MAX_RECORDS, MAX_TITLE_BYTES,
};

/// The root record's `app=`.
pub const APP_NAME: &str = "awman";

/// States awman itself knows, without any agent report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwmanState {
    /// A dialog is waiting on the user → `blocked`, `kind=question`.
    DialogWaiting,
    /// An agent-auth dialog → `blocked`, `kind=auth`.
    AuthDialog,
    /// A command is running → `working`.
    CommandRunning,
    /// The command finished → `done`.
    CommandDone,
    /// The command failed → `error`.
    CommandFailed,
}

/// One agent slot on one tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmitSlot {
    /// Slot index within the tab (the `<slot>` in `tab<N>/<slot>`).
    pub slot: usize,
    /// The agent's own 0141 status, when it reports.
    pub status: Option<EffectiveStatus>,
    /// awman-originated state for this slot.
    pub awman: Option<AwmanState>,
}

/// One TUI tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmitTab {
    /// 1-based tab number (the `<N>` in `tab<N>`).
    pub index: usize,
    /// The tab label, used as the child record's `title`.
    pub label: String,
    pub slots: Vec<EmitSlot>,
}

/// Everything the projection reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmitInput {
    pub tabs: Vec<EmitTab>,
}

/// The projection itself. Stateless: the coalescing (only on change, at most
/// four writes per second) is the Layer 3 emitter's.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProgramStatusProjection;

impl ProgramStatusProjection {
    /// The records to emit: one root (`app=awman`, aggregated by
    /// `blocked > error > working > idle > done`, `msg` from the winner) plus
    /// one child per agent slot (`id=tab<N>/<slot>`, `title` = tab label,
    /// `msg` = the agent's sanitised message; `kind`/`progress` on
    /// `blocked`). Caps enforced here: 128-byte `id`, 192-byte `title`,
    /// 2048-byte `msg`, at most 64 records; truncation keeps valid UTF-8.
    ///
    /// A slot with neither an agent status nor an awman state has no child
    /// record. With no records at all the root is `idle`. A slot's state is:
    /// a waiting dialog (`blocked`) first, because the user must act; then a
    /// finished command (`done`/`error`), because the agent is gone; then the
    /// agent's own report; then `working` for a running command.
    ///
    /// Child records are whole replacements, so the emitter clears removed
    /// children (simplest: write [`Self::clear`] before the new set).
    pub fn reports(&self, state: &EmitInput) -> Vec<ProgramStatusReport> {
        let mut children = Vec::new();
        let mut winner: Option<ProgramStatusReport> = None;
        for tab in &state.tabs {
            let title = Some(sanitize(&tab.label, MAX_TITLE_BYTES)).filter(|t| !t.is_empty());
            for slot in &tab.slots {
                let Some(resolved) = resolve_slot(slot) else {
                    continue;
                };
                let id = format!("tab{}/{}", tab.index, slot.slot);
                if id.len() > MAX_ID_BYTES {
                    continue;
                }
                let child = resolved.report(Some(id), None, title.clone());
                if winner
                    .as_ref()
                    .is_none_or(|best| priority(best) < priority(&child))
                {
                    winner = Some(child.clone());
                }
                if children.len() < MAX_RECORDS - 1 {
                    children.push(child);
                }
            }
        }

        // Aggregate every slot, including children beyond the wire record cap.
        let root = match winner {
            Some(winner) => ProgramStatusReport {
                id: None,
                app: Some(APP_NAME.to_owned()),
                title: None,
                ..winner.clone()
            },
            None => ProgramStatusReport {
                id: None,
                state: ReportState::State(ProgramState::Idle),
                kind: None,
                progress: None,
                app: Some(APP_NAME.to_owned()),
                title: None,
                msg: None,
            },
        };
        let mut reports = Vec::with_capacity(children.len() + 1);
        reports.push(root);
        reports.extend(children);
        reports
    }

    /// The one record that clears everything (`state=clear`, no `id`), for
    /// normal exit and the panic hook.
    pub fn clear(&self) -> ProgramStatusReport {
        ProgramStatusReport {
            id: None,
            state: crate::data::program_status::ReportState::Clear,
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: None,
        }
    }
}

/// One slot's displayed status.
struct ResolvedSlot {
    state: ProgramState,
    kind: Option<BlockedKind>,
    progress: Option<u8>,
    msg: Option<String>,
}

impl ResolvedSlot {
    fn report(
        self,
        id: Option<String>,
        app: Option<String>,
        title: Option<String>,
    ) -> ProgramStatusReport {
        let blocked = self.state == ProgramState::Blocked;
        ProgramStatusReport {
            id,
            state: ReportState::State(self.state),
            kind: self.kind.filter(|_| blocked),
            progress: self.progress.filter(|p| blocked && *p <= 100),
            app,
            title,
            msg: self
                .msg
                .map(|m| sanitize(&m, MAX_MSG_BYTES))
                .filter(|m| !m.is_empty()),
        }
    }
}

fn resolve_slot(slot: &EmitSlot) -> Option<ResolvedSlot> {
    let from_agent = |status: &EffectiveStatus| ResolvedSlot {
        state: status.state,
        kind: status.kind,
        progress: status.progress,
        msg: status.msg.clone(),
    };
    // The agent's message, kept only when the agent agrees on the state.
    let agent_msg = |state: ProgramState| {
        slot.status
            .as_ref()
            .filter(|s| s.state == state)
            .and_then(|s| s.msg.clone())
    };
    let fixed = |state: ProgramState, kind: Option<BlockedKind>| ResolvedSlot {
        state,
        kind,
        progress: None,
        msg: agent_msg(state),
    };
    match slot.awman {
        Some(AwmanState::DialogWaiting) => Some(ResolvedSlot {
            msg: None,
            ..fixed(ProgramState::Blocked, Some(BlockedKind::Question))
        }),
        Some(AwmanState::AuthDialog) => Some(ResolvedSlot {
            msg: None,
            ..fixed(ProgramState::Blocked, Some(BlockedKind::Auth))
        }),
        Some(AwmanState::CommandDone) => Some(fixed(ProgramState::Done, None)),
        Some(AwmanState::CommandFailed) => Some(fixed(ProgramState::Error, None)),
        Some(AwmanState::CommandRunning) => Some(
            slot.status
                .as_ref()
                .map(from_agent)
                .unwrap_or_else(|| fixed(ProgramState::Working, None)),
        ),
        None => slot.status.as_ref().map(from_agent),
    }
}

fn priority(report: &ProgramStatusReport) -> u8 {
    match report.state {
        ReportState::State(state) => state.priority(),
        ReportState::Clear => 0,
    }
}

/// Drop control characters, strip invisible/bidi characters, then cut to
/// `max` bytes on a character boundary.
fn sanitize(text: &str, max: usize) -> String {
    let visible: String = text
        .chars()
        .filter(|c| !matches!(*c as u32, 0x00..=0x1F | 0x7F..=0x9F))
        .collect();
    let mut clean = strip_invisible(&visible);
    if clean.len() > max {
        let mut end = max;
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean.truncate(end);
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(state: ProgramState, msg: Option<&str>) -> Option<EffectiveStatus> {
        Some(EffectiveStatus {
            state,
            kind: (state == ProgramState::Blocked).then_some(BlockedKind::Permission),
            progress: Some(50),
            msg: msg.map(str::to_owned),
            app: Some("claude".into()),
            id: None,
        })
    }

    fn slot(slot: usize, status: Option<EffectiveStatus>, awman: Option<AwmanState>) -> EmitSlot {
        EmitSlot {
            slot,
            status,
            awman,
        }
    }

    fn tab(index: usize, label: &str, slots: Vec<EmitSlot>) -> EmitTab {
        EmitTab {
            index,
            label: label.into(),
            slots,
        }
    }

    fn state_of(r: &ProgramStatusReport) -> ProgramState {
        match r.state {
            ReportState::State(s) => s,
            ReportState::Clear => panic!("unexpected clear"),
        }
    }

    #[test]
    fn empty_input_is_an_idle_root() {
        let reports = ProgramStatusProjection.reports(&EmitInput::default());
        assert_eq!(reports.len(), 1);
        assert_eq!(state_of(&reports[0]), ProgramState::Idle);
        assert_eq!(reports[0].app.as_deref(), Some(APP_NAME));
        assert_eq!(reports[0].id, None);
    }

    #[test]
    fn root_aggregates_by_priority_with_the_winners_message() {
        let order = [
            ProgramState::Done,
            ProgramState::Idle,
            ProgramState::Working,
            ProgramState::Error,
            ProgramState::Blocked,
        ];
        // Each prefix of the ascending list: the last state wins.
        for n in 1..=order.len() {
            let slots = order[..n]
                .iter()
                .enumerate()
                .map(|(i, s)| slot(i, agent(*s, Some(s.as_str())), None))
                .collect();
            let reports = ProgramStatusProjection.reports(&EmitInput {
                tabs: vec![tab(1, "t", slots)],
            });
            let root = &reports[0];
            assert_eq!(state_of(root), order[n - 1]);
            assert_eq!(root.msg.as_deref(), Some(order[n - 1].as_str()));
            assert_eq!(reports.len(), n + 1);
        }
        // Mixed across tabs, in reverse order: blocked still wins.
        let reports = ProgramStatusProjection.reports(&EmitInput {
            tabs: vec![
                tab(
                    1,
                    "a",
                    vec![slot(0, agent(ProgramState::Blocked, Some("perm?")), None)],
                ),
                tab(
                    2,
                    "b",
                    vec![slot(0, agent(ProgramState::Error, Some("boom")), None)],
                ),
            ],
        });
        assert_eq!(state_of(&reports[0]), ProgramState::Blocked);
        assert_eq!(reports[0].kind, Some(BlockedKind::Permission));
        assert_eq!(reports[0].progress, Some(50));
        assert_eq!(reports[0].msg.as_deref(), Some("perm?"));
    }

    #[test]
    fn children_carry_ids_titles_and_kind_progress_only_on_blocked() {
        let reports = ProgramStatusProjection.reports(&EmitInput {
            tabs: vec![
                tab(
                    1,
                    "build",
                    vec![slot(
                        0,
                        agent(ProgramState::Working, Some("compiling")),
                        None,
                    )],
                ),
                tab(
                    3,
                    "review",
                    vec![
                        slot(0, None, None),
                        slot(2, agent(ProgramState::Blocked, Some("allow?")), None),
                    ],
                ),
            ],
        });
        assert_eq!(reports.len(), 3);
        let working = &reports[1];
        assert_eq!(working.id.as_deref(), Some("tab1/0"));
        assert_eq!(working.title.as_deref(), Some("build"));
        assert_eq!(working.msg.as_deref(), Some("compiling"));
        assert_eq!((working.kind, working.progress), (None, None));
        assert_eq!(working.app, None);
        let blocked = &reports[2];
        assert_eq!(blocked.id.as_deref(), Some("tab3/2"));
        assert_eq!(blocked.title.as_deref(), Some("review"));
        assert_eq!(blocked.kind, Some(BlockedKind::Permission));
        assert_eq!(blocked.progress, Some(50));
        for r in &reports[1..] {
            assert!(crate::data::program_status::is_valid_id(
                r.id.as_deref().unwrap()
            ));
        }
    }

    #[test]
    fn awman_originated_states() {
        let cases = [
            (
                None,
                Some(AwmanState::DialogWaiting),
                ProgramState::Blocked,
                Some(BlockedKind::Question),
            ),
            (
                agent(ProgramState::Working, None),
                Some(AwmanState::AuthDialog),
                ProgramState::Blocked,
                Some(BlockedKind::Auth),
            ),
            (
                None,
                Some(AwmanState::CommandRunning),
                ProgramState::Working,
                None,
            ),
            (
                agent(ProgramState::Blocked, None),
                Some(AwmanState::CommandRunning),
                ProgramState::Blocked,
                Some(BlockedKind::Permission),
            ),
            (
                agent(ProgramState::Working, None),
                Some(AwmanState::CommandDone),
                ProgramState::Done,
                None,
            ),
            (
                None,
                Some(AwmanState::CommandFailed),
                ProgramState::Error,
                None,
            ),
        ];
        for (status, awman, state, kind) in cases {
            let reports = ProgramStatusProjection.reports(&EmitInput {
                tabs: vec![tab(1, "t", vec![slot(0, status, awman)])],
            });
            assert_eq!(state_of(&reports[1]), state, "{awman:?}");
            assert_eq!(reports[1].kind, kind, "{awman:?}");
        }
        // A failed command keeps the agent's own error message.
        let reports = ProgramStatusProjection.reports(&EmitInput {
            tabs: vec![tab(
                1,
                "t",
                vec![slot(
                    0,
                    agent(ProgramState::Error, Some("tests failed")),
                    Some(AwmanState::CommandFailed),
                )],
            )],
        });
        assert_eq!(reports[1].msg.as_deref(), Some("tests failed"));
    }

    #[test]
    fn caps_and_utf8_safe_truncation() {
        let label = format!("\u{202E}tab\x1b]52;{}", "é".repeat(200));
        let msg = "€".repeat(1000);
        let slots = (0..100)
            .map(|i| slot(i, agent(ProgramState::Working, Some(&msg)), None))
            .collect();
        let reports = ProgramStatusProjection.reports(&EmitInput {
            tabs: vec![tab(1, &label, slots)],
        });
        assert_eq!(reports.len(), MAX_RECORDS);
        let child = &reports[1];
        let title = child.title.as_deref().unwrap();
        assert!(title.len() <= MAX_TITLE_BYTES);
        assert!(title.starts_with("tab]52;é"));
        assert!(!title.contains('\u{202E}') && !title.contains('\x1b'));
        let m = child.msg.as_deref().unwrap();
        assert!(m.len() <= MAX_MSG_BYTES && m.len() > MAX_MSG_BYTES - 3);
        assert!(m.chars().all(|c| c == '€'));
    }

    #[test]
    fn clear_is_state_clear_without_id() {
        let clear = ProgramStatusProjection.clear();
        assert_eq!(clear.state, ReportState::Clear);
        assert_eq!(clear.id, None);
    }
}

#[cfg(test)]
mod cap_regression_tests {
    use super::*;
    #[test]
    fn root_includes_high_priority_agents_beyond_the_record_cap() {
        let mut input = EmitInput {
            tabs: vec![EmitTab {
                index: 1,
                label: "all".into(),
                slots: (0..MAX_RECORDS)
                    .map(|slot| EmitSlot {
                        slot,
                        status: None,
                        awman: Some(AwmanState::CommandDone),
                    })
                    .collect(),
            }],
        };
        input.tabs[0].slots.last_mut().unwrap().awman = Some(AwmanState::AuthDialog);
        let reports = ProgramStatusProjection.reports(&input);
        assert_eq!(reports.len(), MAX_RECORDS);
        assert_eq!(reports[0].state, ReportState::State(ProgramState::Blocked));
        assert_eq!(reports[0].kind, Some(BlockedKind::Auth));
    }
}
