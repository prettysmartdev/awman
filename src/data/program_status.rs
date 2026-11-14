//! OSC 7501 — the Program Status Protocol (draft rev 0.4) — as pure data.
//!
//! A program running in a PTY writes `ESC ] 7501 ; key=value:key=value … ST`
//! to tell its terminal whether it is `idle`, `working`, `done`, `blocked`
//! (on `permission`, `question` or `auth`) or in `error`. awman is that
//! terminal for every agent it runs (WI 0141). This module owns the protocol
//! model: the report shape, the body parser and its validation rules, and the
//! per-agent record set with its priority aggregate. It has no I/O and knows
//! nothing about byte streams; the stream scanner that finds sequences in
//! agent output is Layer 1 (`engine::agent_runtime::program_status`).
//!
//! Spellings on every wire (config, CLI JSON, API events) are the spec's own,
//! exposed through `as_str`/`parse` in the style of `data::session_status`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The OSC number this protocol uses.
pub const OSC_NUMBER: &str = "7501";

/// Longest `id` value (whole path), in bytes.
pub const MAX_ID_BYTES: usize = 128;
/// Deepest `id` path.
pub const MAX_ID_SEGMENTS: usize = 8;
/// Longest single `id` segment, in bytes.
pub const MAX_ID_SEGMENT_BYTES: usize = 32;
/// Longest pair key, in bytes (keys match `[a-z]+`).
pub const MAX_KEY_BYTES: usize = 16;
/// Longest base64 `msg`, checked **before** decoding.
pub const MAX_MSG_ENCODED_BYTES: usize = 2732;
/// Longest decoded `msg`, in bytes.
pub const MAX_MSG_BYTES: usize = 2048;
/// Longest base64 `title`, checked **before** decoding.
pub const MAX_TITLE_ENCODED_BYTES: usize = 256;
/// Longest decoded `title`, in bytes.
pub const MAX_TITLE_BYTES: usize = 192;
/// Records kept per agent; least-recently-updated is evicted beyond this.
pub const MAX_RECORDS: usize = 64;
/// Longest whole sequence (introducer to terminator) anyone buffers.
pub const MAX_SEQUENCE_BYTES: usize = 4096;

/// What a program says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl ProgramState {
    pub const ALL: &'static [ProgramState] = &[
        ProgramState::Idle,
        ProgramState::Working,
        ProgramState::Done,
        ProgramState::Blocked,
        ProgramState::Error,
    ];

    /// The spec spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ProgramState::Idle => "idle",
            ProgramState::Working => "working",
            ProgramState::Done => "done",
            ProgramState::Blocked => "blocked",
            ProgramState::Error => "error",
        }
    }

    /// Exact, lower-case match on the spec spelling.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|state| state.as_str() == s)
    }

    /// Aggregation rank: `blocked > error > working > idle > done`. Higher
    /// wins in [`ProgramStatusSet::effective`].
    pub fn priority(self) -> u8 {
        match self {
            ProgramState::Blocked => 4,
            ProgramState::Error => 3,
            ProgramState::Working => 2,
            ProgramState::Idle => 1,
            ProgramState::Done => 0,
        }
    }

    /// `blocked` and `error`: the workflow never advances past these.
    pub fn is_problem(self) -> bool {
        matches!(self, ProgramState::Blocked | ProgramState::Error)
    }
}

impl std::fmt::Display for ProgramState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a program is `blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

impl BlockedKind {
    pub const ALL: &'static [BlockedKind] = &[
        BlockedKind::Permission,
        BlockedKind::Question,
        BlockedKind::Auth,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Auth => "auth",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.as_str() == s)
    }
}

impl std::fmt::Display for BlockedKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `state=` of one report: a program state, or `clear`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportState {
    State(ProgramState),
    Clear,
}

/// One accepted OSC 7501 report, decoded and sanitised.
///
/// `msg` and `title` are untrusted agent text: already validated (no C0, DEL
/// or C1 control characters) and stripped of bidi overrides and invisible
/// formatting characters. They are only ever rendered outside the terminal
/// grid and never written back into any PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatusReport {
    /// Path grammar (`a/b/c`); `None` is the root record.
    pub id: Option<String>,
    pub state: ReportState,
    /// Only kept for `Blocked`.
    pub kind: Option<BlockedKind>,
    /// `0..=100`, only for `Working`/`Blocked`.
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

/// What one OSC 7501 body parsed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedOsc7501 {
    /// `7501;?` — the feature-detection query.
    Query,
    Report(ProgramStatusReport),
    /// Malformed, unknown state, or otherwise invalid: changes nothing.
    Ignored,
}

/// Parse the bytes between `7501;` and the terminator.
///
/// Rules (spec §parsing): `key=value` pairs separated by `:`, whitespace
/// trimmed; a malformed pair is skipped; unknown keys are ignored; the last
/// repeated value wins; a missing or unknown `state` → `Ignored`; an unknown
/// `kind` or out-of-range `progress` or invalid `app` is treated as absent; a
/// malformed `id` → `Ignored` (never a fallback to root); `msg`/`title`
/// encoded and decoded caps are checked before and after base64 decoding; a
/// control character anywhere in decoded text → `Ignored`.
pub fn parse_body(body: &[u8]) -> ParsedOsc7501 {
    if body == b"?" {
        return ParsedOsc7501::Query;
    }

    let mut pairs = BTreeMap::<String, String>::new();
    for pair in body.split(|byte| *byte == b':') {
        let Some(equal) = pair.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let Ok(key) = std::str::from_utf8(&pair[..equal]) else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || key.len() > MAX_KEY_BYTES
            || !key.bytes().all(|b| b.is_ascii_lowercase())
        {
            continue;
        }
        if !matches!(
            key,
            "id" | "state" | "msg" | "title" | "kind" | "progress" | "app"
        ) {
            continue;
        }
        // Keep invalid known values: the last id/state/text must never
        // silently reuse an earlier value or fall back to the root record.
        let value = std::str::from_utf8(&pair[equal + 1..])
            .unwrap_or("\u{fffd}")
            .trim();
        pairs.insert(key.to_owned(), value.to_owned());
    }

    let Some(state_text) = pairs.get("state") else {
        return ParsedOsc7501::Ignored;
    };
    let state = if state_text == "clear" {
        ReportState::Clear
    } else if let Some(state) = ProgramState::parse(state_text) {
        ReportState::State(state)
    } else {
        return ParsedOsc7501::Ignored;
    };
    let id = pairs.get("id").cloned();
    if id.as_deref().is_some_and(|id| !is_valid_id(id)) {
        return ParsedOsc7501::Ignored;
    }

    let decode_text = |key: &str, encoded_cap: usize, decoded_cap: usize| {
        let value = pairs.get(key)?;
        if value.len() > encoded_cap {
            return Some(Err(()));
        }
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value));
        let decoded = match decoded {
            Ok(decoded) if decoded.len() <= decoded_cap => decoded,
            _ => return Some(Err(())),
        };
        let text = match std::str::from_utf8(&decoded) {
            Ok(text) => text,
            Err(_) => return Some(Err(())),
        };
        if text.chars().any(is_forbidden_control) {
            return Some(Err(()));
        }
        Some(Ok(strip_invisible(text)))
    };

    let msg = match decode_text("msg", MAX_MSG_ENCODED_BYTES, MAX_MSG_BYTES) {
        Some(Ok(value)) => Some(value),
        Some(Err(())) => return ParsedOsc7501::Ignored,
        None => None,
    };
    let title = match decode_text("title", MAX_TITLE_ENCODED_BYTES, MAX_TITLE_BYTES) {
        Some(Ok(value)) => Some(value),
        Some(Err(())) => return ParsedOsc7501::Ignored,
        None => None,
    };

    let state_value = match state {
        ReportState::State(state) => Some(state),
        ReportState::Clear => None,
    };
    let kind = if state_value == Some(ProgramState::Blocked) {
        pairs
            .get("kind")
            .and_then(|value| BlockedKind::parse(value))
    } else {
        None
    };
    let progress = if matches!(
        state_value,
        Some(ProgramState::Working | ProgramState::Blocked)
    ) {
        pairs
            .get("progress")
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|v| *v <= 100)
    } else {
        None
    };
    let app = pairs.get("app").filter(|value| valid_app(value)).cloned();

    ParsedOsc7501::Report(ProgramStatusReport {
        id,
        state,
        kind,
        progress,
        app,
        title,
        msg,
    })
}

/// Remove bidi overrides and invisible formatting characters
/// (U+202A–U+202E, U+2066–U+2069, U+200B–U+200F, U+FEFF).
pub fn strip_invisible(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !matches!(*c as u32,
                0x202A..=0x202E | 0x2066..=0x2069 | 0x200B..=0x200F | 0xFEFF
            )
        })
        .collect()
}

/// Whether `id` is a valid record path (`[A-Za-z0-9_.+-]{1,32}` segments,
/// at most [`MAX_ID_SEGMENTS`] deep and [`MAX_ID_BYTES`] long).
pub fn is_valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return false;
    }
    let segments: Vec<_> = id.split('/').collect();
    segments.len() <= MAX_ID_SEGMENTS
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_ID_SEGMENT_BYTES
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
        })
}

fn is_forbidden_control(ch: char) -> bool {
    matches!(ch as u32, 0x00..=0x1F | 0x7F | 0x80..=0x9F)
}

fn valid_app(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
}

/// The last accepted report for one id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatusRecord {
    pub state: ProgramState,
    pub kind: Option<BlockedKind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
    /// Monotonic update counter from the owning set; drives LRU eviction.
    pub updated: u64,
}

/// One agent's aggregate status: the winning record by priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveStatus {
    pub state: ProgramState,
    pub kind: Option<BlockedKind>,
    pub progress: Option<u8>,
    pub msg: Option<String>,
    /// From the winning record, or inherited from its nearest ancestor.
    pub app: Option<String>,
    /// Which record won; `None` for the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl EffectiveStatus {
    /// Same `state` and `kind`: consumers treat anything else (progress,
    /// msg) as display-only and never as a transition.
    pub fn same_transition(&self, other: &EffectiveStatus) -> bool {
        self.state == other.state && self.kind == other.kind
    }

    /// The status a tab shows for several agents: the highest
    /// [`ProgramState::priority`] wins.
    pub fn most_urgent<'a>(
        statuses: impl IntoIterator<Item = &'a EffectiveStatus>,
    ) -> Option<&'a EffectiveStatus> {
        statuses.into_iter().max_by_key(|s| s.state.priority())
    }
}

/// Every record one agent has reported, keyed by id (the root is `""`).
///
/// `apply` replaces a record wholesale, so keys missing from a new report are
/// dropped. `Clear` with an id removes it and its descendants; `Clear` without
/// an id removes everything. The set is capped at [`MAX_RECORDS`] with
/// least-recently-updated eviction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramStatusSet {
    records: BTreeMap<String, ProgramStatusRecord>,
    tick: u64,
}

impl ProgramStatusSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one accepted report in. Returns `true` when the set changed.
    pub fn apply(&mut self, report: &ProgramStatusReport) -> bool {
        if let ReportState::Clear = report.state {
            let Some(id) = report.id.as_deref() else {
                let changed = !self.records.is_empty();
                self.records.clear();
                if changed {
                    self.tick = self.tick.wrapping_add(1);
                }
                return changed;
            };
            if !is_valid_id(id) {
                return false;
            }
            let key = id.to_owned();
            let prefix = format!("{id}/");
            let before = self.records.len();
            self.records
                .retain(|candidate, _| candidate != &key && !candidate.starts_with(&prefix));
            let changed = before != self.records.len();
            if changed {
                self.tick = self.tick.wrapping_add(1);
            }
            return changed;
        }
        if report.id.as_deref().is_some_and(|id| !is_valid_id(id)) {
            return false;
        }
        let ReportState::State(state) = report.state else {
            unreachable!()
        };
        self.tick = self.tick.wrapping_add(1);
        let key = report.id.clone().unwrap_or_default();
        self.records.insert(
            key,
            ProgramStatusRecord {
                state,
                kind: if state == ProgramState::Blocked {
                    report.kind
                } else {
                    None
                },
                progress: if matches!(state, ProgramState::Working | ProgramState::Blocked) {
                    report.progress.filter(|p| *p <= 100)
                } else {
                    None
                },
                app: report.app.clone(),
                title: report.title.clone(),
                msg: report.msg.clone(),
                updated: self.tick,
            },
        );
        if self.records.len() > MAX_RECORDS {
            if let Some(oldest) = self
                .records
                .iter()
                .min_by_key(|(id, record)| (record.updated, *id))
                .map(|(id, _)| id.clone())
            {
                self.records.remove(&oldest);
            }
        }
        true
    }

    /// RIS (`ESC c`): drop every record.
    pub fn reset(&mut self) {
        self.records.clear();
    }

    /// The aggregate, by `blocked > error > working > idle > done`; `None`
    /// when the agent holds no records.
    pub fn effective(&self) -> Option<EffectiveStatus> {
        let (id, record) = self
            .records
            .iter()
            .max_by_key(|(id, record)| (record.state.priority(), record.updated, *id))?;
        let app = record.app.clone().or_else(|| {
            let mut parent = id.as_str();
            while let Some((ancestor, _)) = parent.rsplit_once('/') {
                if let Some(app) = self.records.get(ancestor).and_then(|r| r.app.clone()) {
                    return Some(app);
                }
                parent = ancestor;
            }
            self.records.get("").and_then(|root| root.app.clone())
        });
        Some(EffectiveStatus {
            state: record.state,
            kind: record.kind,
            progress: record.progress,
            msg: record.msg.clone(),
            app,
            id: if id.is_empty() {
                None
            } else {
                Some(id.clone())
            },
        })
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Read access for tests and the TUI overview.
    pub fn records(&self) -> &BTreeMap<String, ProgramStatusRecord> {
        &self.records
    }

    /// Updates applied so far (every `apply` that changed the set).
    pub fn tick(&self) -> u64 {
        self.tick
    }
}
