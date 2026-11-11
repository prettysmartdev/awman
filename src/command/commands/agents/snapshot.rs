//! Shared agent data for `agents status`, `list` and `detail`.
//!
//! Every frontend (CLI table, TUI dashboard, `--json`, the API) renders from
//! these types, so the columns and their contents agree by construction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::command::error::CommandError;
use crate::data::program_status::{EffectiveStatus, ProgramState};
use crate::data::session::AgentHandle;
use crate::engine::agent_runtime::AgentRuntimeEngine;

/// Column headings for `agents status`, in the order of
/// [`AgentSnapshot::status_cells`]. The first column is the state glyph.
pub const STATUS_COLUMNS: &[&str] = &[
    "", "AGENT ID", "NAME", "SOURCE", "IMAGE", "UPTIME", "CPU", "MEM",
];

/// Column headings for `agents list`, in the order of
/// [`AgentSnapshot::list_cells`].
pub const LIST_COLUMNS: &[&str] = &["AGENT ID", "NAME", "IMAGE", "STATUS", "UPTIME", "SOURCE"];

/// How many characters of the runtime id `short_id` keeps (docker-style).
const SHORT_ID_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Running,
    /// Running, but the owning TUI tab reports it silent for too long. Only
    /// ever applied to agents that do not report their own status (OSC 7501).
    Stuck,
    /// The agent reported `blocked` (permission, question or auth): it needs
    /// the user.
    Waiting,
    /// The agent reported `error`.
    Errored,
    /// The agent reported `done` (or `idle`: its turn has ended).
    Done,
    /// Stopped but still present in the runtime (`agents ls --all`).
    Stopped,
}

impl AgentState {
    /// Distinct state glyphs remain readable without colour.
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Running => "●",
            Self::Stuck => "▲",
            Self::Waiting => "◆",
            Self::Errored => "✗",
            Self::Done => "✓",
            Self::Stopped => "○",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentState::Running => "running",
            AgentState::Stuck => "stuck",
            AgentState::Waiting => "waiting",
            AgentState::Errored => "errored",
            AgentState::Done => "done",
            AgentState::Stopped => "stopped",
        }
    }

    /// Display order: agents that need attention first (waiting and errored
    /// agents, then stuck ones), then the rest.
    fn sort_rank(&self) -> u8 {
        match self {
            AgentState::Waiting | AgentState::Errored => 0,
            AgentState::Stuck => 1,
            AgentState::Running => 2,
            AgentState::Done => 3,
            AgentState::Stopped => 4,
        }
    }
}

/// Where an agent came from, derived from its **name** — never a label —
/// so the marker works identically on every runtime tier (Apple cannot read
/// labels back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum AgentSource {
    /// An interactive `awman` session agent.
    Session,
    /// Launched by squad for the named task.
    Squad(String),
}

impl AgentSource {
    /// Classify by name, the one identity channel every runtime tier honours
    /// (Docker `--filter name=`, Apple client-side prefix, sandbox `sbx ls`
    /// prefix).
    pub fn classify(name: &str) -> Self {
        match crate::engine::container::naming::parse_squad_task_slug(name) {
            Some(slug) => AgentSource::Squad(slug.to_string()),
            None => AgentSource::Session,
        }
    }

    /// `"session"` or `"squad:<slug>"`.
    pub fn label(&self) -> String {
        match self {
            AgentSource::Session => "session".to_string(),
            AgentSource::Squad(slug) => format!("squad:{slug}"),
        }
    }
}

/// One agent, as every `agents` subcommand presents it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// The full runtime id.
    pub id: String,
    /// The first 12 characters of `id`, docker-style.
    pub short_id: String,
    pub name: String,
    pub image: String,
    pub state: AgentState,
    pub started_at: DateTime<Utc>,
    /// Computed once, at collection time.
    pub uptime_secs: u64,
    pub source: AgentSource,
    /// The TUI tab running this agent. TUI only.
    pub tab_number: Option<u32>,
    pub command_label: Option<String>,
    /// `None` when the runtime does not measure stats or the lookup failed.
    pub cpu_percent: Option<f64>,
    pub memory_mb: Option<f64>,
    /// The `blocked` kind (`permission`, `question`, `auth`) the agent
    /// reported, when it is waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The agent's own one-line message (already sanitised; untrusted text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

impl AgentSnapshot {
    pub fn from_handle(handle: &AgentHandle, state: AgentState, now: DateTime<Utc>) -> Self {
        let uptime_secs = (now - handle.started_at).num_seconds().max(0) as u64;
        Self {
            id: handle.id.clone(),
            short_id: short_id(&handle.id),
            name: handle.name.clone(),
            image: handle.image_tag.clone(),
            state,
            started_at: handle.started_at,
            uptime_secs,
            source: AgentSource::classify(&handle.name),
            tab_number: None,
            command_label: None,
            cpu_percent: None,
            memory_mb: None,
            kind: None,
            msg: None,
        }
    }

    /// Mark the TUI tab running this agent (matched on container name): its
    /// tab number, command label, and `Stuck` when the tab reports it stuck.
    pub fn apply_tui_context(&mut self, tui: Option<&AgentsTuiContext>) {
        let Some(tab) = tui.and_then(|ctx| ctx.tab_for(&self.name)) else {
            return;
        };
        self.tab_number = Some(tab.tab_number);
        self.command_label = Some(tab.command_label.clone());
        if self.state != AgentState::Running {
            return;
        }
        match &tab.program_status {
            // A reporting agent is judged on its report, never on silence.
            Some(status) => self.apply_program_status(status),
            None if tab.is_stuck => self.state = AgentState::Stuck,
            None => {}
        }
    }

    /// Present an agent's reported status: the state, plus the blocked kind
    /// and the agent's message for the JSON output and the STATUS column.
    fn apply_program_status(&mut self, status: &EffectiveStatus) {
        self.state = match status.state {
            ProgramState::Blocked => AgentState::Waiting,
            ProgramState::Error => AgentState::Errored,
            ProgramState::Done | ProgramState::Idle => AgentState::Done,
            ProgramState::Working => AgentState::Running,
        };
        self.kind = match status.state {
            ProgramState::Blocked => status.kind.map(|k| k.as_str().to_string()),
            _ => None,
        };
        self.msg = status.msg.clone();
    }

    /// The STATUS column text: `running`, or e.g. `waiting (permission)`.
    pub fn status_cell(&self) -> String {
        match &self.kind {
            Some(kind) => format!("{} ({kind})", self.state.label()),
            None => self.state.label().to_string(),
        }
    }

    /// `source.label()`, or `tab N` when the agent belongs to a TUI tab.
    pub fn source_cell(&self) -> String {
        match (&self.source, self.tab_number) {
            (AgentSource::Session, Some(tab)) => format!("tab {tab}"),
            (source, _) => source.label(),
        }
    }

    /// `"12.3%"` or `"-"`.
    pub fn cpu_cell(&self) -> String {
        self.cpu_percent
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "-".into())
    }

    /// `"512 MB"` or `"-"`.
    pub fn mem_cell(&self) -> String {
        self.memory_mb.map(format_mb).unwrap_or_else(|| "-".into())
    }

    pub fn uptime_cell(&self) -> String {
        humanize_uptime(self.uptime_secs)
    }

    /// Cells in [`STATUS_COLUMNS`] order (glyph first).
    pub fn status_cells(&self) -> Vec<String> {
        vec![
            self.state.glyph().to_string(),
            self.short_id.clone(),
            self.name.clone(),
            self.source_cell(),
            self.image.clone(),
            self.uptime_cell(),
            self.cpu_cell(),
            self.mem_cell(),
        ]
    }

    /// Cells in [`LIST_COLUMNS`] order. `no_trunc` prints the full id.
    pub fn list_cells(&self, no_trunc: bool) -> Vec<String> {
        vec![
            if no_trunc {
                self.id.clone()
            } else {
                self.short_id.clone()
            },
            self.name.clone(),
            self.image.clone(),
            self.status_cell(),
            self.uptime_cell(),
            self.source_cell(),
        ]
    }
}

/// Aggregate counts and resource totals for the dashboard header.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentTotals {
    pub running: usize,
    pub stuck: usize,
    /// Agents that reported `blocked`.
    #[serde(default)]
    pub waiting: usize,
    /// Agents that reported `error`.
    #[serde(default)]
    pub errored: usize,
    /// Agents that reported `done`.
    #[serde(default)]
    pub done: usize,
    pub stopped: usize,
    /// `None` when no row reported a value.
    pub cpu_percent: Option<f64>,
    pub memory_mb: Option<f64>,
}

impl AgentTotals {
    fn from_agents(agents: &[AgentSnapshot]) -> Self {
        let mut totals = AgentTotals::default();
        for agent in agents {
            match agent.state {
                AgentState::Running => totals.running += 1,
                AgentState::Stuck => totals.stuck += 1,
                AgentState::Waiting => totals.waiting += 1,
                AgentState::Errored => totals.errored += 1,
                AgentState::Done => totals.done += 1,
                AgentState::Stopped => totals.stopped += 1,
            }
            if let Some(cpu) = agent.cpu_percent {
                *totals.cpu_percent.get_or_insert(0.0) += cpu;
            }
            if let Some(mem) = agent.memory_mb {
                *totals.memory_mb.get_or_insert(0.0) += mem;
            }
        }
        totals
    }
}

/// Everything `agents status` shows, from one collection pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentsOverview {
    /// `runtime_name()`.
    pub runtime: String,
    /// `display_name()`.
    pub runtime_display: String,
    pub available: bool,
    /// The availability error text when `available == false`.
    pub unavailable_reason: Option<String>,
    pub agents: Vec<AgentSnapshot>,
    pub totals: AgentTotals,
    pub collected_at: DateTime<Utc>,
    /// Chosen once per run, so a watch footer does not flicker between tips.
    pub tip: String,
}

impl AgentsOverview {
    /// `Docker · available · 3 running · 1 stuck · CPU 12.3% · Mem 512 MB`,
    /// or `Docker · unavailable: <reason>`. The header text every frontend
    /// shows.
    pub fn header_line(&self) -> String {
        if !self.available {
            return format!(
                "{} · unavailable: {}",
                self.runtime_display,
                self.unavailable_reason
                    .as_deref()
                    .unwrap_or("unknown error")
            );
        }
        let mut parts = vec![
            self.runtime_display.clone(),
            "available".to_string(),
            format!("{} running", self.totals.running),
        ];
        if self.totals.waiting > 0 {
            parts.push(format!("{} waiting", self.totals.waiting));
        }
        if self.totals.errored > 0 {
            parts.push(format!("{} errored", self.totals.errored));
        }
        if self.totals.stuck > 0 {
            parts.push(format!("{} stuck", self.totals.stuck));
        }
        if self.totals.stopped > 0 {
            parts.push(format!("{} stopped", self.totals.stopped));
        }
        if let Some(cpu) = self.totals.cpu_percent {
            parts.push(format!("CPU {cpu:.1}%"));
        }
        if let Some(mem) = self.totals.memory_mb {
            parts.push(format!("Mem {}", format_mb(mem)));
        }
        parts.join(" · ")
    }

    /// What to show instead of a table when no agent is running.
    pub fn empty_state_lines(&self) -> Vec<String> {
        vec![
            format!("No agents running on {}.", self.runtime_display),
            "Start one with `awman chat` or `awman exec workflow <file>`.".to_string(),
        ]
    }
}

/// Live TUI tab data used to mark which tab owns an agent.
///
/// **Deliberately a per-tick frontend hook** (WI 0114 F-49, which proposed
/// folding it into `CallerContext`). Unlike `is_local_user_session`, this
/// is not a fact about the caller fixed at construction: `awman agents
/// status --watch` reads it once per tick, and the tab numbers, stuck flags
/// and command labels it carries change while the command runs. A
/// construction-time snapshot would pin the dashboard to whatever the tabs
/// looked like when the watch started. The frontend supplies data here, not
/// a decision, which is the line Tenet 2 draws.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentsTuiContext {
    pub tabs: Vec<TuiTabSnapshot>,
}

impl AgentsTuiContext {
    pub fn new(tabs: Vec<TuiTabSnapshot>) -> Self {
        Self { tabs }
    }

    fn tab_for(&self, container_name: &str) -> Option<&TuiTabSnapshot> {
        self.tabs
            .iter()
            .find(|t| t.container_name.as_deref() == Some(container_name))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiTabSnapshot {
    pub tab_number: u32,
    pub container_name: Option<String>,
    pub is_stuck: bool,
    /// The tab's agent-reported status (OSC 7501), when it reports one. A
    /// reporting agent is never `Stuck`.
    pub program_status: Option<EffectiveStatus>,
    pub command_label: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CollectOptions {
    /// Append `list_stopped()` rows as `Stopped`.
    pub include_stopped: bool,
    /// Fetch per-agent CPU/memory (`status`); `list` skips it to stay fast.
    pub with_stats: bool,
}

/// The one collection function for `status` and `list`.
///
/// - An `availability()` failure is not an `Err`: the overview comes back with
///   `available: false`, no rows and `unavailable_reason` filled. One-shot
///   callers turn that into the runtime's error; `--watch` keeps ticking.
/// - Stats are fetched only when `with_stats` and the runtime reports them;
///   a stats error leaves that row's values `None`.
/// - Sorted stuck first, then running by uptime (longest first), then stopped.
pub fn collect_overview(
    runtime: &dyn AgentRuntimeEngine,
    tui: Option<&AgentsTuiContext>,
    opts: CollectOptions,
    tip: &str,
) -> Result<AgentsOverview, CommandError> {
    match runtime.availability() {
        Ok(()) => collect_available(runtime, tui, opts, tip),
        Err(e) => Ok(AgentsOverview::unavailable(runtime, e.to_string(), tip)),
    }
}

impl AgentsOverview {
    /// An overview with no rows that says the runtime cannot be reached.
    pub fn unavailable(runtime: &dyn AgentRuntimeEngine, reason: String, tip: &str) -> Self {
        Self {
            runtime: runtime.runtime_name().to_string(),
            runtime_display: runtime.display_name().to_string(),
            available: false,
            unavailable_reason: Some(reason),
            agents: Vec::new(),
            totals: AgentTotals::default(),
            collected_at: Utc::now(),
            tip: tip.to_string(),
        }
    }
}

/// [`collect_overview`] without the availability check, for one-shot callers
/// that already checked it and want its typed error (and exit class).
pub(super) fn collect_available(
    runtime: &dyn AgentRuntimeEngine,
    tui: Option<&AgentsTuiContext>,
    opts: CollectOptions,
    tip: &str,
) -> Result<AgentsOverview, CommandError> {
    let now = Utc::now();
    let mut overview = AgentsOverview {
        runtime: runtime.runtime_name().to_string(),
        runtime_display: runtime.display_name().to_string(),
        available: true,
        unavailable_reason: None,
        agents: Vec::new(),
        totals: AgentTotals::default(),
        collected_at: now,
        tip: tip.to_string(),
    };

    let fetch_stats = opts.with_stats && runtime.capabilities().reports_resource_stats;
    let mut agents: Vec<AgentSnapshot> = runtime
        .list_running_all()?
        .iter()
        .take_while(|_| crate::engine::host_cli::check_collection_cancel().is_ok())
        .map(|handle| {
            let mut snapshot = AgentSnapshot::from_handle(handle, AgentState::Running, now);
            if fetch_stats {
                if let Ok(stats) = runtime.stats(handle) {
                    snapshot.cpu_percent = Some(stats.cpu_percent);
                    snapshot.memory_mb = Some(stats.memory_mb);
                }
            }
            snapshot.apply_tui_context(tui);
            snapshot
        })
        .collect();

    if opts.include_stopped {
        agents.extend(
            runtime
                .list_stopped()?
                .iter()
                .map(|handle| AgentSnapshot::from_handle(handle, AgentState::Stopped, now)),
        );
    }

    sort_agents(&mut agents);
    overview.totals = AgentTotals::from_agents(&agents);
    overview.agents = agents;
    Ok(overview)
}

/// Stuck first, then by uptime descending, stopped last.
pub(super) fn sort_agents(agents: &mut [AgentSnapshot]) {
    agents.sort_by(|a, b| {
        a.state
            .sort_rank()
            .cmp(&b.state.sort_rank())
            .then(b.uptime_secs.cmp(&a.uptime_secs))
    });
}

/// `42s`, `4m12s`, `2h03m`, `3d01h`.
pub fn humanize_uptime(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if secs < MINUTE {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m{:02}s", secs / MINUTE, secs % MINUTE)
    } else if secs < DAY {
        format!("{}h{:02}m", secs / HOUR, (secs % HOUR) / MINUTE)
    } else {
        format!("{}d{:02}h", secs / DAY, (secs % DAY) / HOUR)
    }
}

pub(super) fn short_id(id: &str) -> String {
    id.chars().take(SHORT_ID_LEN).collect()
}

fn format_mb(mb: f64) -> String {
    format!("{mb:.0} MB")
}
