//! Shared agent data for `agents status`, `list` and `detail`.
//!
//! Every frontend (CLI table, TUI dashboard, `--json`, the API) renders from
//! these types, so the columns and their contents agree by construction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::command::error::CommandError;
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
    /// Running, but the owning TUI tab reports it silent for too long.
    Stuck,
    /// Stopped but still present in the runtime (`agents ls --all`).
    Stopped,
}

impl AgentState {
    /// Distinct state glyphs remain readable without colour.
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Running => "●",
            Self::Stuck => "▲",
            Self::Stopped => "○",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentState::Running => "running",
            AgentState::Stuck => "stuck",
            AgentState::Stopped => "stopped",
        }
    }

    /// Display order: stuck agents need attention first.
    fn sort_rank(&self) -> u8 {
        match self {
            AgentState::Stuck => 0,
            AgentState::Running => 1,
            AgentState::Stopped => 2,
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
        if tab.is_stuck && self.state == AgentState::Running {
            self.state = AgentState::Stuck;
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
            self.state.label().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_context_new_stores_tabs() {
        let ctx = AgentsTuiContext::new(vec![TuiTabSnapshot {
            tab_number: 1,
            container_name: Some("awman-abc".into()),
            is_stuck: false,
            command_label: "chat".into(),
        }]);
        assert_eq!(ctx.tabs.len(), 1);
        assert_eq!(ctx.tabs[0].tab_number, 1);
    }

    #[test]
    fn classify_source_marks_squad_agents() {
        assert_eq!(AgentSource::classify("awman-abc"), AgentSource::Session);
        assert_eq!(
            AgentSource::classify("awman-squad-issue-triage-12ab34cd"),
            AgentSource::Squad("issue-triage".to_string())
        );
        assert_eq!(
            AgentSource::Squad("issue-triage".into()).label(),
            "squad:issue-triage"
        );
    }

    use crate::command::commands::agents::test_support::{handle, FakeRuntime};

    const OPTS: CollectOptions = CollectOptions {
        include_stopped: false,
        with_stats: true,
    };

    fn tab(n: u32, container: &str, stuck: bool, label: &str) -> TuiTabSnapshot {
        TuiTabSnapshot {
            tab_number: n,
            container_name: Some(container.to_string()),
            is_stuck: stuck,
            command_label: label.to_string(),
        }
    }

    #[test]
    fn collect_overview_counts_totals_and_states() {
        let runtime = FakeRuntime::new()
            .with_running(vec![
                handle("aaa", "awman-a", 100),
                handle("bbb", "awman-b", 50),
                handle("ccc", "awman-c", 10),
            ])
            .with_stopped(vec![handle("ddd", "awman-d", 500)]);
        let tui = AgentsTuiContext::new(vec![tab(2, "awman-c", true, "chat")]);
        let opts = CollectOptions {
            include_stopped: true,
            ..OPTS
        };
        let o = collect_overview(&runtime, Some(&tui), opts, "tip").unwrap();
        assert!(o.available);
        assert_eq!(o.runtime, "fake");
        assert_eq!(o.runtime_display, "Fake Runtime");
        assert_eq!(o.tip, "tip");
        assert_eq!(o.totals.running, 2);
        assert_eq!(o.totals.stuck, 1);
        assert_eq!(o.totals.stopped, 1);
        // Three running agents report 10% CPU / 100 MB each; stopped rows none.
        assert_eq!(o.totals.cpu_percent, Some(30.0));
        assert_eq!(o.totals.memory_mb, Some(300.0));
        assert_eq!(o.agents.len(), 4);
    }

    #[test]
    fn collect_overview_sorts_stuck_first_then_uptime_descending() {
        let runtime = FakeRuntime::new()
            .with_running(vec![
                handle("a1", "awman-short", 10),
                handle("b2", "awman-long", 900),
                handle("c3", "awman-stuck", 5),
            ])
            .with_stopped(vec![handle("d4", "awman-old", 9999)]);
        let tui = AgentsTuiContext::new(vec![tab(1, "awman-stuck", true, "chat")]);
        let opts = CollectOptions {
            include_stopped: true,
            ..OPTS
        };
        let o = collect_overview(&runtime, Some(&tui), opts, "").unwrap();
        let names: Vec<&str> = o.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["awman-stuck", "awman-long", "awman-short", "awman-old"]
        );
    }

    #[test]
    fn collect_overview_stats_error_leaves_values_none() {
        let runtime = FakeRuntime::new().with_running(vec![
            handle("a1", "awman-ok", 10),
            handle("b2", "awman-bad", 5),
        ]);
        runtime.stats_fail.lock().unwrap().push("awman-bad".into());
        let o = collect_overview(&runtime, None, OPTS, "").unwrap();
        let bad = o.agents.iter().find(|a| a.name == "awman-bad").unwrap();
        let ok = o.agents.iter().find(|a| a.name == "awman-ok").unwrap();
        assert_eq!((bad.cpu_percent, bad.memory_mb), (None, None));
        assert_eq!(bad.cpu_cell(), "-");
        assert_eq!(bad.mem_cell(), "-");
        assert_eq!(ok.cpu_percent, Some(10.0));
        // Totals only add what rows reported.
        assert_eq!(o.totals.cpu_percent, Some(10.0));
    }

    #[test]
    fn collect_overview_runtime_without_stats_renders_not_reported() {
        // SBX-style runtimes report zeros; capabilities say not to ask.
        let mut runtime = FakeRuntime::new().with_running(vec![handle("a1", "awman-a", 10)]);
        runtime.caps.reports_resource_stats = false;
        let o = collect_overview(&runtime, None, OPTS, "").unwrap();
        assert!(!runtime.called("stats"), "stats must not be queried");
        assert_eq!(o.agents[0].cpu_cell(), "-");
        assert_eq!(o.agents[0].mem_cell(), "-");
        assert_eq!(o.totals.cpu_percent, None);
        assert_eq!(o.totals.memory_mb, None);
        assert!(!o.header_line().contains("CPU"));
    }

    #[test]
    fn collect_overview_without_stats_option_skips_stats() {
        let runtime = FakeRuntime::new().with_running(vec![handle("a1", "awman-a", 10)]);
        let opts = CollectOptions {
            with_stats: false,
            ..OPTS
        };
        let o = collect_overview(&runtime, None, opts, "").unwrap();
        assert!(!runtime.called("stats"));
        assert_eq!(o.agents[0].cpu_percent, None);
    }

    #[test]
    fn collect_overview_unavailable_runtime_is_not_an_error() {
        let runtime = FakeRuntime::new();
        *runtime.available.lock().unwrap() = Err("daemon down".into());
        let o = collect_overview(&runtime, None, OPTS, "t").unwrap();
        assert!(!o.available);
        assert!(o.agents.is_empty());
        assert!(o
            .unavailable_reason
            .as_deref()
            .unwrap()
            .contains("daemon down"));
        assert!(o.header_line().starts_with("Fake Runtime · unavailable: "));
        assert!(!runtime.called("list_running_all"));
    }

    #[test]
    fn collect_overview_tui_context_enriches_row_with_matching_tab() {
        let runtime = FakeRuntime::new().with_running(vec![
            handle("a1", "awman-tab", 10),
            handle("b2", "awman-other", 5),
        ]);
        let tui = AgentsTuiContext::new(vec![tab(3, "awman-tab", false, "chat claude")]);
        let o = collect_overview(&runtime, Some(&tui), OPTS, "").unwrap();
        let row = o.agents.iter().find(|a| a.name == "awman-tab").unwrap();
        assert_eq!(row.tab_number, Some(3));
        assert_eq!(row.command_label.as_deref(), Some("chat claude"));
        assert_eq!(row.state, AgentState::Running);
        assert_eq!(row.source_cell(), "tab 3");
        let other = o.agents.iter().find(|a| a.name == "awman-other").unwrap();
        assert_eq!(other.tab_number, None);
        assert_eq!(other.command_label, None);
        assert_eq!(other.source_cell(), "session");
    }

    #[test]
    fn collect_overview_tui_context_marks_stuck_tab_stuck() {
        let runtime = FakeRuntime::new().with_running(vec![handle("a1", "awman-tab", 10)]);
        let tui = AgentsTuiContext::new(vec![tab(1, "awman-tab", true, "chat")]);
        let o = collect_overview(&runtime, Some(&tui), OPTS, "").unwrap();
        assert_eq!(o.agents[0].state, AgentState::Stuck);
        assert_eq!(o.totals.stuck, 1);
        assert_eq!(o.totals.running, 0);
        assert!(o.header_line().contains("1 stuck"));
    }

    #[test]
    fn collect_overview_without_tui_context_leaves_rows_plain() {
        let runtime = FakeRuntime::new().with_running(vec![handle("a1", "awman-tab", 10)]);
        let o = collect_overview(&runtime, None, OPTS, "").unwrap();
        assert_eq!(o.agents[0].tab_number, None);
        assert_eq!(o.agents[0].state, AgentState::Running);
    }

    #[test]
    fn collect_overview_classifies_session_vs_squad_source() {
        let runtime = FakeRuntime::new().with_running(vec![
            handle("a1", "awman-abc", 20),
            handle("b2", "awman-squad-issue-triage-12ab34cd", 10),
        ]);
        let o = collect_overview(&runtime, None, OPTS, "").unwrap();
        let squad = o.agents.iter().find(|a| a.id == "b2").unwrap();
        let session = o.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(squad.source, AgentSource::Squad("issue-triage".into()));
        assert_eq!(squad.source_cell(), "squad:issue-triage");
        assert_eq!(session.source, AgentSource::Session);
        assert_eq!(session.source_cell(), "session");
    }

    #[test]
    fn snapshot_short_id_is_twelve_chars_and_cells_follow_columns() {
        let h = handle("0123456789abcdef", "awman-x", 65);
        let s = AgentSnapshot::from_handle(&h, AgentState::Running, Utc::now());
        assert_eq!(s.short_id, "0123456789ab");
        assert_eq!(s.status_cells().len(), STATUS_COLUMNS.len());
        assert_eq!(s.list_cells(false).len(), LIST_COLUMNS.len());
        assert_eq!(s.list_cells(false)[0], "0123456789ab");
        assert_eq!(s.list_cells(true)[0], "0123456789abcdef");
    }

    #[test]
    fn empty_state_names_the_runtime() {
        let runtime = FakeRuntime::new();
        let o = collect_overview(&runtime, None, OPTS, "").unwrap();
        assert!(o.empty_state_lines()[0].contains("Fake Runtime"));
        assert_eq!(o.header_line(), "Fake Runtime · available · 0 running");
    }

    #[test]
    fn humanize_uptime_picks_two_units() {
        assert_eq!(humanize_uptime(42), "42s");
        assert_eq!(humanize_uptime(252), "4m12s");
        assert_eq!(humanize_uptime(2 * 3600 + 3 * 60 + 9), "2h03m");
        assert_eq!(humanize_uptime(3 * 86400 + 3600), "3d01h");
    }
}
