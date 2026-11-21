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
