//! Serializable instance state, independent of channels and rendering.
use crate::data::{
    acp_protocol::{PermissionRequest, SessionUpdate},
    annotations::Annotations,
    dialog::DialogRequest,
    frontend_protocol::{FrontendKindWire, FrontendRole, TabKindWire, ViewScope},
    ids::*,
    message::MessageLevel,
    program_status::EffectiveStatus,
    session::{AgentHandle, InvocationStatus, SessionId, SessionState, SessionType},
    workflow_state::{CountdownKind, WorkflowSummary},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

mod optional_base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => {
                serializer.serialize_some(&base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let encoded = Option::<String>::deserialize(deserializer)?;
        encoded
            .map(|value| {
                base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ExecutionPhase {
    Idle,
    Running { command: String },
    Done { command: String, exit_code: i32 },
    Error { command: String, message: String },
}

impl ExecutionPhase {
    /// The command this phase is about, or `""` when idle.
    ///
    /// Every non-idle variant carries it: by the time a tab polls the
    /// command's result, `refresh_from_session` has usually already moved the
    /// phase from `Running` to `Done`/`Error`, so reading only `Running`
    /// loses the name.
    pub fn command_name(&self) -> &str {
        match self {
            Self::Idle => "",
            Self::Running { command }
            | Self::Done { command, .. }
            | Self::Error { command, .. } => command,
        }
    }

    /// How a session's recorded command reads in the tab bar and the
    /// execution window.
    ///
    /// The phase is a *view* of `SessionState::current_command`, which Layer 2
    /// writes for every frontend (decision Q3, WI 0114 F-22). The TUI used to
    /// keep its own copy and set it in four places, so the tab could disagree
    /// with the session about what was running.
    pub fn of_session_state(state: &SessionState) -> Self {
        let Some(cmd) = state.current_command.as_ref() else {
            return Self::Idle;
        };
        match &cmd.status {
            InvocationStatus::Pending | InvocationStatus::Running => Self::Running {
                command: cmd.subcommand.clone(),
            },
            InvocationStatus::Done => Self::Done {
                command: cmd.subcommand.clone(),
                exit_code: cmd.exit_code.unwrap_or(0),
            },
            InvocationStatus::Error(message) => Self::Error {
                command: cmd.subcommand.clone(),
                message: message.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerWindowState {
    Hidden,
    Minimized,
    Maximized,
}

impl ContainerWindowState {
    pub fn cycle(self) -> Self {
        match self {
            Self::Hidden => Self::Maximized,
            Self::Minimized => Self::Maximized,
            Self::Maximized => Self::Minimized,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorkflowViewState {
    pub steps: Vec<WorkflowStepView>,
    pub current_step: Option<String>,
    /// Effective step-concurrency cap for the running workflow (WI-0096 §11),
    /// set by the frontend when the engine reports a parallel group start.
    /// `None` means unlimited — the overview caps parallel rows at the legacy 3
    /// and renders no "queued" markers, so behavior is unchanged.
    pub max_concurrent: Option<usize>,
    /// Each step's agent-reported status label (`blocked: permission`,
    /// `error`, `done`), by step name. Shown under the step box.
    pub reported: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorkflowStepView {
    pub name: String,
    pub status: StepViewStatus,
    /// Resolved agent (e.g. `"claude"`) — fed by `report_workflow_progress`.
    pub agent: Option<String>,
    /// Optional resolved model.
    pub model: Option<String>,
    /// Steps this one waits on. Drives the column-grouping in the overview
    /// renderer (steps with the same sorted `depends_on` set sit in the
    /// same topological column).
    pub depends_on: Vec<String>,
    /// Which phase this step belongs to. `Setup`/`Teardown` steps get their
    /// own dedicated first/last column in the overview rather than being
    /// grouped by `depends_on` topology alongside `Agent` steps.
    pub kind: WorkflowStepKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepViewStatus {
    /// Not started.
    Pending,
    /// Executing now.
    Running,
    /// An `on_failure` remediation is running for this step.
    Fixing,
    /// Finished successfully.
    Done,
    /// Finished with a failure.
    Error,
    /// Abandoned before it could finish.
    Cancelled,
    /// Never ran because it was not needed.
    Skipped,
}

impl StepViewStatus {
    /// Whether the step will not run again.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Skipped)
    }

    /// How an agent step's Layer 0 state reads.
    pub fn of_step_state(state: &crate::data::workflow_state::StepState) -> Self {
        use crate::data::workflow_state::StepState;
        match state {
            StepState::Pending => Self::Pending,
            StepState::Running { .. } => Self::Running,
            StepState::Succeeded => Self::Done,
            StepState::Failed { .. } => Self::Error,
            StepState::Cancelled => Self::Cancelled,
            StepState::Skipped => Self::Skipped,
        }
    }

    /// How a setup/teardown step's Layer 0 state reads.
    pub fn of_phase_step_status(status: &crate::data::workflow_state::PhaseStepStatus) -> Self {
        use crate::data::workflow_state::PhaseStepStatus;
        match status {
            PhaseStepStatus::Pending => Self::Pending,
            PhaseStepStatus::Running => Self::Running,
            PhaseStepStatus::Succeeded => Self::Done,
            PhaseStepStatus::Failed { .. } => Self::Error,
            PhaseStepStatus::Remediating { .. } => Self::Fixing,
            PhaseStepStatus::Skipped => Self::Skipped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStepKind {
    /// A `setup:` step, run once before the main workflow steps.
    Setup,
    /// An ordinary workflow step, grouped into columns by `depends_on`.
    Agent,
    /// A `teardown:` step, run once after the main workflow steps.
    Teardown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct YoloState {
    pub step_name: String,
    pub remaining_secs: u64,
    /// Why the countdown runs; selects the dialog and tab wording.
    pub kind: CountdownKind,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LastContainerSummary {
    pub agent_display_name: String,
    pub container_name: String,
    pub avg_cpu: String,
    pub avg_memory: String,
    pub total_time: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SlotId(pub uuid::Uuid);
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLine {
    pub level: MessageLevel,
    pub text: String,
} // was tui::user_message::StatusLogEntry
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitDiffSummaryWire {
    pub branch: Option<String>,
    pub files: Vec<GitFileChangeWire>,
    pub added: u32,
    pub removed: u32,
} // shape: mirror GitDiffSummary's pub fields exactly
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStatsWire {
    pub name: String,
    pub cpu_percent: f64,
    pub memory_mb: f64,
} // shape: mirror AgentStats
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsDashboardWire {
    pub overview: serde_json::Value, /* AgentsOverview is serde (agents status --json) */
    pub watching: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabLabelWire {
    pub text: String,
    pub color: LabelColorWire,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelColorWire {
    Default,
    Running,
    Done,
    Error,
    Stuck,
    Blocked,
    Yolo,
    Remote,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub session_id: SessionId,
    pub session_type: SessionType,
    pub cwd: PathBuf,
    pub git_root: PathBuf,
    pub default_agent: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpSlotWire {
    pub history: Vec<SessionUpdate>,
    pub pending_permission: Option<PermissionRequest>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotSnapshot {
    pub slot_id: SlotId,
    pub step_name: String,
    pub agent_display_name: String,
    pub model: Option<String>,
    pub container_name: String,
    pub terminal_id: Option<TerminalId>,
    pub acp: Option<AcpSlotWire>,
    pub started_at: DateTime<Utc>,
    pub stats: Option<AgentStatsWire>,
    pub program_status: Option<EffectiveStatus>,
    pub stuck: bool,
    pub countdown_kind: CountdownKind,
    pub yolo: Option<YoloState>,
    pub sandboxed: bool,
    pub phase_step_title: Option<String>,
    pub dormant: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub tab_id: TabId,
    pub session: SessionView,
    pub kind: TabKindWire,
    pub label: TabLabelWire,
    pub execution_phase: ExecutionPhase,
    pub current_workflow: Option<WorkflowSummary>,
    pub current_container: Option<AgentHandle>,
    pub workflow: Option<WorkflowViewState>,
    pub workflow_invocation_id: Option<uuid::Uuid>,
    pub context_path: Option<PathBuf>,
    pub worktree_path: Option<PathBuf>,
    pub slots: Vec<SlotSnapshot>,
    pub focused_slot: Option<SlotId>,
    pub yolo: Option<YoloState>,
    pub yolo_mode: bool,
    pub stuck: bool,
    pub git_summary: Option<GitDiffSummaryWire>,
    pub status_log: Vec<StatusLine>,
    pub agents_dashboard: Option<AgentsDashboardWire>,
    pub last_container_summary: Option<LastContainerSummary>,
    pub annotations: Annotations,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendSummary {
    pub frontend_id: FrontendId,
    pub kind: FrontendKindWire,
    pub role: FrontendRole,
    pub annotations: Annotations,
    pub connected_at: DateTime<Utc>,
    pub view: Option<ViewScope>,
    pub reconnecting: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingDialog {
    pub dialog_id: DialogId,
    pub tab_id: TabId,
    pub request: DialogRequest,
    pub raised_at: DateTime<Utc>,
}
/// The squad indicator's state on the wire: every `SquadHealth` the engine's
/// probe can report, so a relaying presenter colours the dot exactly as the
/// builtin backend would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SquadHealthWire {
    Unknown,
    #[serde(alias = "stopped")]
    NotRunning,
    Unreachable,
    Failed,
    EnvUnmet,
    Running,
    Healthy,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSnapshot {
    pub instance_id: InstanceId,
    pub created_at: DateTime<Utc>,
    pub annotations: Annotations,
    pub primary_frontend_kind: Option<FrontendKindWire>,
    pub frontends: Vec<FrontendSummary>,
    pub tabs: Vec<TabSnapshot>,
    pub active_tab_hint: Option<TabId>,
    pub squad_health: SquadHealthWire,
    pub pending_dialogs: Vec<PendingDialog>,
    pub running_jobs: usize,
}
/// `GET /v1/engine/instances` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSummary {
    pub id: InstanceId,
    pub primary_frontend_kind: Option<FrontendKindWire>,
    pub annotations: Annotations,
    pub tab_count: usize,
    pub running_jobs: usize,
    pub created_at: DateTime<Utc>,
}
/// `GET /v1/engine/instances/{id}`: one instance with its frontends and tabs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceDetail {
    #[serde(flatten)]
    pub summary: InstanceSummary,
    pub frontends: Vec<FrontendSummary>,
    pub tabs: Vec<TabSnapshot>,
}
/// The result of answering a pending dialog. The first answer wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DialogAnswerOutcome {
    Answered,
    /// Someone answered first (`409 dialog_answered`).
    AlreadyAnswered {
        by: crate::data::frontend_protocol::AnsweredBy,
    },
    /// The response does not fit the request (`400 invalid_dialog_response`).
    Rejected,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum AnnotationTarget {
    Instance,
    Tab { tab_id: TabId },
    Terminal { terminal_id: TerminalId },
    Frontend { frontend_id: FrontendId },
}

// A delta is a short-lived wire value; see `EngineFrame`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "delta", content = "value", rename_all = "snake_case")]
pub enum InstanceDelta {
    TabOpened(TabSnapshot),
    TabClosed {
        tab_id: TabId,
    },
    TabLabel {
        tab_id: TabId,
        label: TabLabelWire,
    },
    Phase {
        tab_id: TabId,
        phase: ExecutionPhase,
        current_workflow: Option<WorkflowSummary>,
        current_container: Option<AgentHandle>,
    },
    Workflow {
        tab_id: TabId,
        view: Option<WorkflowViewState>,
        invocation_id: Option<uuid::Uuid>,
        context_path: Option<PathBuf>,
        worktree_path: Option<PathBuf>,
    },
    SlotLaunched {
        tab_id: TabId,
        slot: SlotSnapshot,
    },
    SlotContainerName {
        tab_id: TabId,
        slot_id: SlotId,
        container_name: String,
    },
    SlotExited {
        tab_id: TabId,
        slot_id: SlotId,
        exit_code: Option<i32>,
        /// Final visible terminal grid, sent before the exit is applied so
        /// a slower terminal WebSocket cannot lose the last output frame.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "optional_base64_bytes"
        )]
        final_screen: Option<Vec<u8>>,
    },
    SlotStuck {
        tab_id: TabId,
        slot_id: SlotId,
        stuck: bool,
    },
    SlotStats {
        tab_id: TabId,
        slot_id: SlotId,
        stats: AgentStatsWire,
    },
    SlotCountdownKind {
        tab_id: TabId,
        slot_id: SlotId,
        kind: CountdownKind,
    },
    SlotYolo {
        tab_id: TabId,
        slot_id: SlotId,
        state: Option<YoloState>,
    },
    SlotAcpUpdate {
        tab_id: TabId,
        slot_id: SlotId,
        update: SessionUpdate,
    },
    SlotAcpPermission {
        tab_id: TabId,
        slot_id: SlotId,
        request: Option<PermissionRequest>,
    },
    GroupStarted {
        tab_id: TabId,
    },
    GroupFinished {
        tab_id: TabId,
    },
    FocusedSlot {
        tab_id: TabId,
        slot_id: Option<SlotId>,
    },
    Yolo {
        tab_id: TabId,
        state: Option<YoloState>,
    },
    YoloMode {
        tab_id: TabId,
        on: bool,
    },
    Stuck {
        tab_id: TabId,
        stuck: bool,
    },
    GitSummary {
        tab_id: TabId,
        summary: Option<GitDiffSummaryWire>,
    },
    StatusLine {
        tab_id: TabId,
        line: StatusLine,
    },
    StatusLogReset {
        tab_id: TabId,
    },
    AgentsDashboard {
        tab_id: TabId,
        data: Option<AgentsDashboardWire>,
    },
    LastContainerSummary {
        tab_id: TabId,
        summary: Option<LastContainerSummary>,
    },
    Annotations {
        target: AnnotationTarget,
        annotations: Annotations,
    },
    FrontendAttached(FrontendSummary),
    FrontendDetached {
        frontend_id: FrontendId,
        reason: String,
    },
    SquadHealth(SquadHealthWire),
    ActiveTabHint {
        tab_id: TabId,
    },
    DialogPending(PendingDialog),
    DialogResolved {
        dialog_id: DialogId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileChangeWire {
    pub path: String,
    pub change: GitFileChangeTypeWire,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitFileChangeTypeWire {
    Added,
    Modified,
    Deleted,
}
