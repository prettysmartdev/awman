/**
 * Hand-written TypeScript mirror of awman's Rust serde wire types.
 *
 * Source of truth is the Rust code; every type names its Rust origin with a
 * `// Rust: path::Type` comment. Shapes that do not exist in Rust yet are typed
 * from the binding design (`/awman/context/workflow/design.md`) and marked
 * `// design §x.y (daemon side in progress)`.
 *
 * Serde conventions mirrored here:
 * - `#[serde(tag = "t")]` (internally tagged): the tag sits beside the
 *   variant's fields. A newtype variant `V(Struct)` flattens the struct's
 *   fields beside the tag (e.g. `EngineFrame::Snapshot(InstanceSnapshot)` is
 *   `{"type":"snapshot", ...snapshot fields}`).
 * - `#[serde(tag = "t", content = "c")]` (adjacently tagged): `{t, c}`; unit
 *   variants carry only `{t}`.
 * - Externally tagged unit-or-struct enums (no `tag`): unit variants are a bare
 *   string, others `{ "variant": payload }`.
 * - `Option<T>` without `skip_serializing_if` is always present (as `null`
 *   when `None`) in daemon output → `field: T | null`. On input the field may
 *   be omitted → client-sent types use `field?: T | null`.
 * - `#[serde(skip_serializing_if = "Option::is_none")]` → `field?: T`.
 * - `DateTime<Utc>` → RFC 3339 string. chrono's default serde emits up to
 *   nanosecond precision (`2026-11-09T12:00:00.123456789Z`); `/v1` resource
 *   routes that use `data::wire::timestamp` emit millisecond precision.
 * - Typed ids (`data::ids`) → `"<prefix>_<32 lowercase hex>"` strings.
 *   `Uuid` newtypes (`SlotId`, `SessionId`, invocation ids) → hyphenated UUID strings.
 * - `Vec<u8>` fields with `#[serde(with = "data::terminal::base64_bytes")]` →
 *   STANDARD base64 *with* padding (RFC 4648 §4, not url-safe).
 * - Rust tuples `(A, B)` → two-element JSON arrays; `char` → one-char string.
 */

/* ───────────────────────────── shared primitives ───────────────────────────── */

/** Any JSON value (`serde_json::Value`). */
export type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

/** RFC 3339 UTC timestamp string. */
export type Timestamp = string;

// Rust: data::ids::InstanceId — "ins_<32hex>"
export type InstanceId = string;
// Rust: data::ids::TabId — "tab_<32hex>"
export type TabId = string;
// Rust: data::ids::FrontendId — "fe_<32hex>"
export type FrontendId = string;
// Rust: data::ids::DialogId — "dlg_<32hex>"
export type DialogId = string;
// Rust: data::ids::TerminalId — "term_<32hex>"
export type TerminalId = string;
// Rust: data::ids::TaskId — "task_<32hex>"
export type TaskId = string;
// Rust: data::ids::RunId — "run_<32hex>"
export type RunId = string;
// Rust: data::ids::CommandId — "cmd_<32hex>"
export type CommandId = string;
// Rust: data::ids::LeaseId — "lease_<32hex>"
export type LeaseId = string;
// Rust: data::ids::WorkerSessionId — "ses_<32hex>"
export type WorkerSessionId = string;
// design §1.2/§2.4 (daemon side in progress): browser-link id "blk_<32hex>"
export type BrowserLinkId = string;
// design §2.5 (daemon side in progress): browser grant id "brw_<32hex>"
export type BrowserGrantId = string;
// design §3.1 (daemon side in progress): launch context id "lc_<32hex>"
export type LaunchContextId = string;
// Rust: data::instance_snapshot::SlotId — newtype over Uuid, hyphenated UUID string
export type SlotId = string;
// Rust: data::session::SessionId — newtype over Uuid, hyphenated UUID string
export type SessionId = string;
/** Hyphenated UUID string (`uuid::Uuid`). */
export type Uuid = string;
/** Standard base64 with padding (`data::terminal::base64_bytes`). */
export type Base64 = string;

// Rust: data::annotations::Annotations — serde via BTreeMap<String, String>
export type Annotations = Record<string, string>;

// Rust: data::message::MessageLevel (rename_all = snake_case)
export type MessageLevel = 'info' | 'warning' | 'error' | 'success';

// Rust: data::wire::List<T>
export interface List<T> {
  items: T[];
  next_cursor: string | null;
}

/* ─────────────────────────── awman.frontend.v1 constants ───────────────────── */

// Rust: data::frontend_protocol::FRONTEND_SUBPROTOCOL
export const FRONTEND_PROTOCOL = 'awman.frontend.v1';
// Rust: data::terminal::TERMINAL_SUBPROTOCOL
export const TERMINAL_PROTOCOL = 'awman.terminal.v1';
// design §2.7 (daemon side in progress): ticket rides as a second Sec-WebSocket-Protocol entry "awman.ticket.<awt_…>"
export const TICKET_SUBPROTOCOL_PREFIX = 'awman.ticket.';
// Rust: data::frontend_protocol::MAX_FRAME_BYTES — encoded text-frame size, inclusive
export const MAX_FRAME_BYTES = 1024 * 1024;
// Rust: data::frontend_protocol::SNAPSHOT_CHUNK_BYTES — raw payload target per chunk (server uses SNAPSHOT_CHUNK_BYTES - 256)
export const SNAPSHOT_CHUNK_BYTES = 768 * 1024;
// Rust: data::frontend_protocol::MAX_SNAPSHOT_BYTES — server-side cap on a reassembled snapshot
export const MAX_SNAPSHOT_BYTES = 64 * 1024 * 1024;
// design §5.4: the web client caps reassembly at 16 MiB (stricter than the Rust 64 MiB cap)
export const CLIENT_MAX_SNAPSHOT_BYTES = 16 * 1024 * 1024;
// Rust: data::frontend_protocol::PRIMARY_RECONNECT_GRACE (2 s) — tui/cli primaries
export const PRIMARY_RECONNECT_GRACE_MS = 2_000;
// design §3.5 (daemon side in progress): data::frontend_protocol::WEB_PRIMARY_RECONNECT_GRACE (60 s) — web primaries
export const WEB_PRIMARY_RECONNECT_GRACE_MS = 60_000;
// Rust: data::frontend_protocol::RECONNECT_BACKOFF_INITIAL
export const RECONNECT_BACKOFF_INITIAL_MS = 50;
// Rust: data::frontend_protocol::RECONNECT_BACKOFF_MAX
export const RECONNECT_BACKOFF_MAX_MS = 1_000;
// Rust: frontend::api::frontend_socket::serve_frontend_socket — Hello must arrive within 5 s of the upgrade
export const HELLO_TIMEOUT_MS = 5_000;
// design §2.8 (daemon side in progress): browser Hello bounded to 16 KiB
export const BROWSER_HELLO_MAX_BYTES = 16 * 1024;
// Rust: frontend::api::terminals::MAX_CLIENT_MESSAGE_BYTES — one terminal client frame (base64 input inflates 4/3)
export const MAX_TERMINAL_CLIENT_MESSAGE_BYTES = 256 * 1024;
// design §2.8/§5.5: browser terminal Input chunks ≤ 64 KiB raw; paste > 1 MiB refused
export const TERMINAL_INPUT_CHUNK_BYTES = 64 * 1024;
export const TERMINAL_PASTE_MAX_BYTES = 1024 * 1024;

/** Rust: data::frontend_protocol::Seq (u64; first frame of each direction is seq 1). */
export type Seq = number;

/* ─────────────────────────── awman.frontend.v1 shared ──────────────────────── */

// Rust: data::frontend_protocol::FrontendKindWire (rename_all = snake_case)
// `web` — design §3.5 (daemon side in progress: FrontendKindWire::Web)
export type FrontendKindWire = 'tui' | 'cli' | 'web';

// Rust: data::frontend_protocol::FrontendRole
export type FrontendRole = 'primary' | 'secondary';

// Rust: data::frontend_protocol::ViewScope — externally tagged: "all" | {"tab": "tab_…"}
export type ViewScope = 'all' | { tab: TabId };

// Rust: data::frontend_protocol::TermSize
export interface TermSize {
  cols: number;
  rows: number;
}

// Rust: data::frontend_protocol::TabKindWire (default normal)
export type TabKindWire = 'normal' | 'squad';

// Rust: data::frontend_protocol::StartupSpawn (default none)
export type StartupSpawn = 'none' | 'ready' | 'agents_status_watch';

// Rust: data::frontend_protocol::OpenTabOptions — no field-level serde(default): kind, annotations, startup are required on the wire
export interface OpenTabOptions {
  kind: TabKindWire;
  label?: string | null;
  annotations: Annotations;
  startup: StartupSpawn;
  lease_id?: LeaseId | null;
}

// Rust: data::frontend_protocol::SubmitInput (tag = "source")
export type SubmitInput =
  | { source: 'command_box'; raw: string }
  | { source: 'argv'; args: string[] }
  | { source: 'lease'; lease_id: LeaseId; args: string[] };

// Rust: data::worker::queue — instance-owned WI143 queue over awman.frontend.v1.
export interface QueueEntry {
  id: string; server: string; title: string; summary: string | null; kind: string;
  labels: Record<string, string>; priority: number; age_ms: number; for_you: boolean;
  body: string | null; source_url: string | null;
}
export interface QueueSnapshot {
  items: QueueEntry[]; for_you: number; new_since_seen: number;
  error: string | null; loaded: boolean;
}
export interface PickRun {
  lease_id: LeaseId; title: string; server: string; cwd: string;
  command_line: string; argv: string[];
}
export type QueueRequest =
  | { operation: 'snapshot' | 'refresh' | 'seen' }
  | { operation: 'pick'; server: string; item_id: string }
  | { operation: 'checkout_offer'; lease_id: LeaseId }
  | { operation: 'checkout'; lease_id: LeaseId; choice: 'worktree' | 'fresh_clone' }
  | { operation: 'finish'; lease_id: LeaseId; choice: 'release' | 'cancel' | 'keep_running' };
export type QueueReply =
  | { result: 'snapshot'; value: QueueSnapshot }
  | { result: 'picked'; value: PickRun }
  | { result: 'checkout_offer'; value: boolean }
  | { result: 'done' }
  | { result: 'error'; value: { code: 'ineligible' | 'taken' | 'failed'; error?: string } };

// Rust: data::frontend_protocol::WorkflowRequestWire (tag = "request")
export type WorkflowRequestWire = { request: 'open_control_board'; step_name: string };

// Rust: data::frontend_protocol::ContainerWindowStateWire
export type ContainerWindowStateWire = 'hidden' | 'minimized' | 'maximized';

// Rust: data::frontend_protocol::UiRequest (tag = "request", rename_all = snake_case)
export type UiRequest =
  | { request: 'open_tab'; dir: string }
  | { request: 'next_tab' }
  | { request: 'prev_tab' }
  | { request: 'close_tab'; tab_id: TabId }
  | { request: 'set_container_window'; tab_id: TabId; slot_id: SlotId; state: ContainerWindowStateWire }
  | { request: 'open_overview'; tab_id: TabId }
  | { request: 'focus_workflow_window'; tab_id: TabId }
  | { request: 'focus_work'; target_id: string };

// Rust: data::frontend_protocol::UiOutcome
export interface UiOutcome {
  claimed_by_host: boolean;
  claimed_by: string | null;
  detail: string | null;
}

// Rust: data::frontend_protocol::OutputStream
export type OutputStream = 'stdout' | 'stderr';

// Rust: data::frontend_protocol::AnsweredBy (tag = "by")
export type AnsweredBy =
  | { by: 'frontend'; frontend_id: FrontendId }
  | { by: 'api' }
  | { by: 'engine' };

// Rust: data::frontend_protocol::ErrorWire
export interface ErrorWire {
  code: string;
  message: string;
}

// Rust: data::frontend_protocol::CommandResultWire
export interface CommandResultWire {
  exit_code: number;
  outcome: JsonValue | null;
  error: ErrorWire | null;
}

// Rust: data::frontend_protocol::DetachReasonWire (tag = "reason")
export type DetachReasonWire =
  | { reason: 'quit' }
  | { reason: 'engine_disabled' }
  | { reason: 'instance_ended' }
  | { reason: 'replaced' }
  | { reason: 'daemon_shutdown' }
  | { reason: 'protocol_error'; detail: string };

/* ─────────────────────────── client → engine frames ────────────────────────── */

// Rust: data::frontend_protocol::Hello (+ design §3.5 `launch_context`)
// Note: in current Rust `annotations`, `cwd`, `argv`, `env` have no serde(default)
// and are required; design §3.5 makes web send cwd "" / argv [] / env {}.
export interface Hello {
  protocol: typeof FRONTEND_PROTOCOL;
  build: string;
  kind: FrontendKindWire;
  role: FrontendRole;
  annotations: Annotations;
  cwd: string;
  argv: string[];
  /** Rust: HelloEnv(BTreeMap<String,String>) — must be `{}` for web. */
  env: Record<string, string>;
  term_size: TermSize;
  stdin_is_tty: boolean;
  stdout_is_tty: boolean;
  /** Secondary only. */
  view?: ViewScope | null;
  /** Reconnect within the primary grace with the previous frontend id. */
  resume?: FrontendId | null;
  /** design §3.5 (daemon side in progress): required for web primaries without `resume`; forbidden for tui/cli. */
  launch_context?: LaunchContextId | null;
}

// Rust: data::frontend_protocol::FrontendFrame (tag = "type", rename_all = snake_case)
export type FrontendFrame =
  | ({ type: 'hello' } & Hello)
  | { type: 'worker_queue'; request_id: number; tab_id: TabId; request: QueueRequest }
  | { type: 'open_tab'; dir: string; options: OpenTabOptions }
  | { type: 'close_tab'; tab_id: TabId }
  | { type: 'focus_tab'; tab_id: TabId }
  | { type: 'submit'; tab_id: TabId; input: SubmitInput; pty_size: TermSize }
  | { type: 'dialog_response'; dialog_id: DialogId; response: DialogResponse }
  | { type: 'cancel'; tab_id: TabId; repeat: boolean }
  | { type: 'cancel_countdown'; tab_id: TabId; step_name?: string | null }
  | { type: 'workflow_request'; tab_id: TabId; request: WorkflowRequestWire }
  | { type: 'activity'; tab_id: TabId }
  | ({ type: 'resize' } & TermSize)
  | ({ type: 'ui_request' } & UiRequest)
  | { type: 'quit' };

export type FrontendFrameType = FrontendFrame['type'];

/* ─────────────────────────── engine → client frames ────────────────────────── */

// Rust: data::frontend_protocol::EngineFrame (tag = "type", rename_all = snake_case)
export type EngineFrame =
  | { type: 'worker_queue'; request_id: number; reply: QueueReply }
  | { type: 'command_bytes'; tab_id: TabId; stream: OutputStream; bytes: Base64 }
  | {
      type: 'welcome';
      frontend_id: FrontendId;
      instance_id: InstanceId;
      protocol: string;
      build: string;
      role: FrontendRole;
    }
  | ({ type: 'snapshot' } & InstanceSnapshot)
  /** `bytes` = base64 of a slice of `serde_json::to_vec(&InstanceSnapshot)`; chunk is 0-based, `of` = total. */
  | { type: 'snapshot_chunk'; chunk: number; of: number; bytes: Base64 }
  | ({ type: 'delta' } & InstanceDelta)
  | { type: 'command_output'; tab_id: TabId; stream: OutputStream; level: MessageLevel; text: string }
  | { type: 'command_finished'; tab_id: TabId; result: CommandResultWire }
  | { type: 'dialog_request'; dialog_id: DialogId; tab_id: TabId; request: DialogRequest }
  | { type: 'dialog_closed'; dialog_id: DialogId; answered_by: AnsweredBy }
  | { type: 'program_status'; tab_id: TabId; slot_id: SlotId; status: EffectiveStatus | null }
  | { type: 'notice'; level: MessageLevel; text: string }
  | { type: 'ui_ack'; request_seq: Seq; outcome: UiOutcome }
  | ({ type: 'detach' } & DetachReasonWire);

export type EngineFrameType = EngineFrame['type'];
export type EngineFrameOf<T extends EngineFrameType> = Extract<EngineFrame, { type: T }>;
export type FrontendFrameOf<T extends FrontendFrameType> = Extract<FrontendFrame, { type: T }>;

// Rust: data::frontend_protocol::Envelope<F> — `{ "seq": n, "type": …, … }`, seq flattened beside the frame
export type Envelope<F> = F & { seq: Seq };

/**
 * Rust: frontend::api::frontend_socket::refuse — NOT an envelope. Sent as a bare
 * text message (no `seq`, no `type`) when Hello is missing/invalid, attach is
 * refused (`AttachRefusal::code`) or an inbound frame is invalid, immediately
 * followed by a WebSocket Close with no status code (1005 at the client).
 */
export interface FrontendSocketRefusal {
  code: AttachRefusalCode | 'protocol_error' | (string & {});
  error: string;
}

// Rust: command::instance::admission::AttachRefusal::code
export type AttachRefusalCode =
  | 'not_found'
  | 'primary_exists'
  | 'protocol_mismatch'
  | 'feature_disabled'
  | 'invalid_annotation'
  | 'instance_start_failed';

/* ───────────────────────────── instance snapshot ───────────────────────────── */

// Rust: data::instance_snapshot::ExecutionPhase (tag = "phase")
export type ExecutionPhase =
  | { phase: 'idle' }
  | { phase: 'running'; command: string }
  | { phase: 'done'; command: string; exit_code: number }
  | { phase: 'error'; command: string; message: string };

// Rust: data::instance_snapshot::ContainerWindowState
export type ContainerWindowState = 'hidden' | 'minimized' | 'maximized';

// Rust: data::instance_snapshot::StepViewStatus
export type StepViewStatus = 'pending' | 'running' | 'fixing' | 'done' | 'error' | 'cancelled' | 'skipped';

// Rust: data::instance_snapshot::WorkflowStepKind
export type WorkflowStepKind = 'setup' | 'agent' | 'teardown';

// Rust: data::instance_snapshot::WorkflowStepView
export interface WorkflowStepView {
  name: string;
  status: StepViewStatus;
  agent: string | null;
  model: string | null;
  depends_on: string[];
  kind: WorkflowStepKind;
}

// Rust: data::instance_snapshot::WorkflowViewState
export interface WorkflowViewState {
  steps: WorkflowStepView[];
  current_step: string | null;
  max_concurrent: number | null;
  /** HashMap<String,String>: step name → agent-reported status label. */
  reported: Record<string, string>;
}

// Rust: data::workflow_state::CountdownKind (rename_all = snake_case, default stuck_step)
export type CountdownKind = 'stuck_step' | 'failure_retry' | 'agent_done' | 'agent_blocked' | 'agent_error';

// Rust: data::instance_snapshot::YoloState
export interface YoloState {
  step_name: string;
  remaining_secs: number;
  kind: CountdownKind;
}

// Rust: data::instance_snapshot::LastContainerSummary
export interface LastContainerSummary {
  agent_display_name: string;
  container_name: string;
  avg_cpu: string;
  avg_memory: string;
  total_time: string;
  exit_code: number;
}

// Rust: data::instance_snapshot::StatusLine
export interface StatusLine {
  level: MessageLevel;
  text: string;
}

// Rust: data::instance_snapshot::GitFileChangeTypeWire
export type GitFileChangeTypeWire = 'added' | 'modified' | 'deleted';

// Rust: data::instance_snapshot::GitFileChangeWire
export interface GitFileChangeWire {
  path: string;
  change: GitFileChangeTypeWire;
  added: number;
  removed: number;
  binary: boolean;
}

// Rust: data::instance_snapshot::GitDiffSummaryWire
export interface GitDiffSummaryWire {
  branch: string | null;
  files: GitFileChangeWire[];
  added: number;
  removed: number;
}

// Rust: data::instance_snapshot::AgentStatsWire
export interface AgentStatsWire {
  name: string;
  cpu_percent: number;
  memory_mb: number;
}

// Rust: data::instance_snapshot::AgentsDashboardWire
export interface AgentsDashboardWire {
  /** `agents status --json` overview, opaque here. */
  overview: JsonValue;
  watching: boolean;
}

// Rust: data::instance_snapshot::LabelColorWire
export type LabelColorWire = 'default' | 'running' | 'done' | 'error' | 'stuck' | 'blocked' | 'yolo' | 'remote';

// Rust: data::instance_snapshot::TabLabelWire
export interface TabLabelWire {
  text: string;
  color: LabelColorWire;
}

// Rust: data::session::SessionType (tag = "type")
export type SessionType =
  | { type: 'local'; workdir: string }
  | { type: 'remote'; repo_url: string; branch: string; cloned_path: string };

// Rust: data::instance_snapshot::SessionView
export interface SessionView {
  session_id: SessionId;
  session_type: SessionType;
  cwd: string;
  git_root: string;
  default_agent: string | null;
  labels: Record<string, string>;
}

// Rust: data::session::AgentHandle
export interface AgentHandle {
  id: string;
  image_tag: string;
  name: string;
  started_at: Timestamp;
}

// Rust: data::workflow_state::WorkflowPhase — NO rename_all: PascalCase on the wire
export type WorkflowPhase = 'Setup' | 'Main' | 'Teardown' | 'Done';

// Rust: data::workflow_state::WorkflowSummary
export interface WorkflowSummary {
  invocation_id: Uuid;
  workflow_name: string;
  work_item: number | null;
  phase: WorkflowPhase;
  total_steps: number;
  completed_steps: number;
  failed_steps: number;
  current_step: string | null;
  updated_at: Timestamp;
}

// Rust: data::program_status::ProgramState
export type ProgramState = 'idle' | 'working' | 'done' | 'blocked' | 'error';
// Rust: data::program_status::BlockedKind
export type BlockedKind = 'permission' | 'question' | 'auth';

// Rust: data::program_status::EffectiveStatus
export interface EffectiveStatus {
  state: ProgramState;
  kind: BlockedKind | null;
  /** u8, 0–100 */
  progress: number | null;
  msg: string | null;
  app: string | null;
  /** skip_serializing_if none: absent for the root record. */
  id?: string;
}

/* ACP (camelCase field names inside these structs) */

// Rust: data::acp_protocol::JsonRpcId (untagged)
export type JsonRpcId = number | string;

// Rust: data::acp_protocol::ContentBlock (tag = "type"; fields are snake_case — no rename_all on fields)
export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; data: string; mime_type: string }
  | { type: 'audio'; data: string; mime_type: string }
  | { type: 'resource_link'; name: string; uri: string; mime_type?: string }
  | { type: 'resource'; resource: JsonValue };

// Rust: data::acp_protocol::ContentChunk (rename_all = camelCase)
export interface ContentChunk {
  content: ContentBlock;
  messageId?: string;
}

// Rust: data::acp_protocol::ToolCallLocation
export interface ToolCallLocation {
  path: string;
  line?: number;
}

// Rust: data::acp_protocol::ToolCall (rename_all = camelCase)
export interface ToolCall {
  toolCallId: string;
  title: string;
  kind?: string;
  status?: string;
  content: JsonValue[];
  locations: ToolCallLocation[];
  rawInput?: JsonValue;
  rawOutput?: JsonValue;
}

// Rust: data::acp_protocol::ToolCallUpdate (rename_all = camelCase)
export interface ToolCallUpdate {
  toolCallId: string;
  kind?: string;
  status?: string;
  title?: string;
  content?: JsonValue[];
  locations?: ToolCallLocation[];
  rawInput?: JsonValue;
  rawOutput?: JsonValue;
}

// Rust: data::acp_protocol::PlanEntry
export interface PlanEntry {
  content: string;
  priority: string;
  status: string;
}

// Rust: data::acp_protocol::AvailableCommand
export interface AvailableCommand {
  name: string;
  description: string;
  input?: JsonValue;
}

// Rust: data::acp_protocol::SessionUpdate (tag = "sessionUpdate", variants snake_case, payload flattened)
export type SessionUpdate =
  | ({ sessionUpdate: 'agent_message_chunk' } & ContentChunk)
  | ({ sessionUpdate: 'agent_thought_chunk' } & ContentChunk)
  | ({ sessionUpdate: 'tool_call' } & ToolCall)
  | ({ sessionUpdate: 'tool_call_update' } & ToolCallUpdate)
  | { sessionUpdate: 'plan'; entries: PlanEntry[] }
  | { sessionUpdate: 'available_commands_update'; available_commands: AvailableCommand[] };

// Rust: data::acp_protocol::PermissionOption (rename_all = camelCase)
export interface PermissionOption {
  optionId: string;
  name: string;
  kind: string;
}

// Rust: data::acp_protocol::PermissionRequest (rename_all = camelCase)
export interface PermissionRequest {
  requestId: JsonRpcId;
  sessionId: string;
  toolCall: ToolCallUpdate;
  options: PermissionOption[];
}

// Rust: data::instance_snapshot::AcpSlotWire
export interface AcpSlotWire {
  history: SessionUpdate[];
  pending_permission: PermissionRequest | null;
}

// Rust: data::instance_snapshot::SlotSnapshot
export interface SlotSnapshot {
  slot_id: SlotId;
  step_name: string;
  agent_display_name: string;
  model: string | null;
  container_name: string;
  terminal_id: TerminalId | null;
  acp: AcpSlotWire | null;
  started_at: Timestamp;
  stats: AgentStatsWire | null;
  program_status: EffectiveStatus | null;
  stuck: boolean;
  countdown_kind: CountdownKind;
  yolo: YoloState | null;
  sandboxed: boolean;
  phase_step_title: string | null;
  dormant: boolean;
}

// Rust: data::instance_snapshot::TabSnapshot
export interface TabSnapshot {
  tab_id: TabId;
  session: SessionView;
  kind: TabKindWire;
  label: TabLabelWire;
  execution_phase: ExecutionPhase;
  current_workflow: WorkflowSummary | null;
  current_container: AgentHandle | null;
  workflow: WorkflowViewState | null;
  workflow_invocation_id: Uuid | null;
  context_path: string | null;
  worktree_path: string | null;
  slots: SlotSnapshot[];
  focused_slot: SlotId | null;
  yolo: YoloState | null;
  yolo_mode: boolean;
  stuck: boolean;
  git_summary: GitDiffSummaryWire | null;
  status_log: StatusLine[];
  agents_dashboard: AgentsDashboardWire | null;
  last_container_summary: LastContainerSummary | null;
  annotations: Annotations;
}

// Rust: data::instance_snapshot::FrontendSummary
export interface FrontendSummary {
  frontend_id: FrontendId;
  kind: FrontendKindWire;
  role: FrontendRole;
  annotations: Annotations;
  connected_at: Timestamp;
  view: ViewScope | null;
  reconnecting: boolean;
}

// Rust: data::instance_snapshot::PendingDialog
export interface PendingDialog {
  dialog_id: DialogId;
  tab_id: TabId;
  request: DialogRequest;
  raised_at: Timestamp;
}

// Rust: data::instance_snapshot::SquadHealthWire
export type SquadHealthWire = 'unknown' | 'running' | 'stopped' | 'unreachable';

// Rust: data::instance_snapshot::InstanceSnapshot
export interface InstanceSnapshot {
  instance_id: InstanceId;
  created_at: Timestamp;
  annotations: Annotations;
  primary_frontend_kind: FrontendKindWire | null;
  frontends: FrontendSummary[];
  tabs: TabSnapshot[];
  active_tab_hint: TabId | null;
  squad_health: SquadHealthWire;
  pending_dialogs: PendingDialog[];
  running_jobs: number;
}

// Rust: data::instance_snapshot::AnnotationTarget (tag = "target")
export type AnnotationTarget =
  | { target: 'instance' }
  | { target: 'tab'; tab_id: TabId }
  | { target: 'terminal'; terminal_id: TerminalId }
  | { target: 'frontend'; frontend_id: FrontendId };

// Rust: data::instance_snapshot::InstanceDelta (tag = "delta", content = "value", rename_all = snake_case)
// Every variant carries content; newtype variants put the inner value directly in `value`.
export type InstanceDelta =
  | { delta: 'tab_opened'; value: TabSnapshot }
  | { delta: 'tab_closed'; value: { tab_id: TabId } }
  | { delta: 'tab_label'; value: { tab_id: TabId; label: TabLabelWire } }
  | {
      delta: 'phase';
      value: {
        tab_id: TabId;
        phase: ExecutionPhase;
        current_workflow: WorkflowSummary | null;
        current_container: AgentHandle | null;
      };
    }
  | {
      delta: 'workflow';
      value: {
        tab_id: TabId;
        view: WorkflowViewState | null;
        invocation_id: Uuid | null;
        context_path: string | null;
        worktree_path: string | null;
      };
    }
  | { delta: 'slot_launched'; value: { tab_id: TabId; slot: SlotSnapshot } }
  | { delta: 'slot_container_name'; value: { tab_id: TabId; slot_id: SlotId; container_name: string } }
  | { delta: 'slot_exited'; value: { tab_id: TabId; slot_id: SlotId; exit_code: number | null } }
  | { delta: 'slot_stuck'; value: { tab_id: TabId; slot_id: SlotId; stuck: boolean } }
  | { delta: 'slot_stats'; value: { tab_id: TabId; slot_id: SlotId; stats: AgentStatsWire } }
  | { delta: 'slot_countdown_kind'; value: { tab_id: TabId; slot_id: SlotId; kind: CountdownKind } }
  | { delta: 'slot_yolo'; value: { tab_id: TabId; slot_id: SlotId; state: YoloState | null } }
  | { delta: 'slot_acp_update'; value: { tab_id: TabId; slot_id: SlotId; update: SessionUpdate } }
  | { delta: 'slot_acp_permission'; value: { tab_id: TabId; slot_id: SlotId; request: PermissionRequest | null } }
  | { delta: 'group_started'; value: { tab_id: TabId } }
  | { delta: 'group_finished'; value: { tab_id: TabId } }
  | { delta: 'focused_slot'; value: { tab_id: TabId; slot_id: SlotId | null } }
  | { delta: 'yolo'; value: { tab_id: TabId; state: YoloState | null } }
  | { delta: 'yolo_mode'; value: { tab_id: TabId; on: boolean } }
  | { delta: 'stuck'; value: { tab_id: TabId; stuck: boolean } }
  | { delta: 'git_summary'; value: { tab_id: TabId; summary: GitDiffSummaryWire | null } }
  | { delta: 'status_line'; value: { tab_id: TabId; line: StatusLine } }
  | { delta: 'status_log_reset'; value: { tab_id: TabId } }
  | { delta: 'agents_dashboard'; value: { tab_id: TabId; data: AgentsDashboardWire | null } }
  | { delta: 'last_container_summary'; value: { tab_id: TabId; summary: LastContainerSummary | null } }
  | { delta: 'annotations'; value: { target: AnnotationTarget; annotations: Annotations } }
  | { delta: 'frontend_attached'; value: FrontendSummary }
  | { delta: 'frontend_detached'; value: { frontend_id: FrontendId; reason: string } }
  | { delta: 'squad_health'; value: SquadHealthWire }
  | { delta: 'active_tab_hint'; value: { tab_id: TabId } }
  | { delta: 'dialog_pending'; value: PendingDialog }
  | { delta: 'dialog_resolved'; value: { dialog_id: DialogId } };

export type InstanceDeltaType = InstanceDelta['delta'];
export type InstanceDeltaOf<T extends InstanceDeltaType> = Extract<InstanceDelta, { delta: T }>;

/* ───────────────────────────────── dialogs ─────────────────────────────────── */

// Rust: data::config::show::ConfigRowKind — externally tagged (rename_all = snake_case)
export type ConfigRowKind =
  | 'scalar'
  | { map_header: { child_prefix: string } }
  | { list_header: { child_prefix: string } }
  | 'map_entry'
  | 'list_entry';

// Rust: data::config::show::ConfigShowRow
export interface ConfigShowRow {
  field: string;
  global: string;
  repo: string;
  effective: string;
  read_only: boolean;
  global_writable: boolean;
  repo_writable: boolean;
  value_hint: string | null;
  shape: ConfigRowKind;
}

// Rust: data::config::show::ConfigShowRejectedEdit
export interface ConfigShowRejectedEdit {
  field: string;
  value: string;
  global: boolean;
  reason: string;
}

// Rust: data::config::show::ConfigEditRequest
export interface ConfigEditRequest {
  field: string;
  value: string;
  global: boolean;
}

// Rust: data::dialog::WorkflowControlBoardState
export interface WorkflowControlBoardState {
  step_name: string;
  can_launch_next: boolean;
  can_continue_current: boolean;
  can_restart: boolean;
  can_go_back: boolean;
  can_finish: boolean;
  continue_unavailable_reason: string | null;
  cancel_to_previous_unavailable_reason: string | null;
  finish_workflow_unavailable_reason: string | null;
  restart_unavailable_reason: string | null;
  can_dismiss: boolean;
  launch_next_label: string | null;
  focused_step_name: string;
  parallel_peer_count: number;
  parallel_peers_running: number;
  failure_lines: string[];
  retry_failed_step: string | null;
  in_parallel_group: boolean;
}

// Rust: data::dialog::WorkflowYoloCountdownState — field `kind` is renamed `countdown_kind`
export interface WorkflowYoloCountdownState {
  step_name: string;
  remaining_secs: number;
  countdown_kind: CountdownKind;
}

// Rust: data::dialog::WorkflowStepConfirmState
export interface WorkflowStepConfirmState {
  completed_step: string;
  next_step: string;
}

// Rust: data::dialog::AgentSetupState
export interface AgentSetupState {
  agent_name: string;
  image_only: boolean;
  has_fallback: boolean;
  fallback_name: string | null;
}

// Rust: data::dialog::MountScopeState
export interface MountScopeState {
  git_root: string;
  cwd: string;
}

// Rust: data::dialog::AgentAuthState
export interface AgentAuthState {
  agent_name: string;
  env_vars: string[];
}

/* workflow prerequisite report (ReviewBody::Prerequisites) */

// Rust: data::workflow_state::PhaseKind (rename_all = lowercase)
export type PhaseKind = 'setup' | 'teardown';

// Rust: data::workflow_prerequisites::PrerequisiteStepKind
export type PrerequisiteStepKind =
  | 'commit_changes'
  | 'push_branch'
  | 'create_pull_request'
  | 'poll_ci'
  | 'pull_branch'
  | 'clone_repo'
  | 'checkout_create_branch'
  | 'run_shell'
  | 'run_script';

// Rust: data::workflow_prerequisites::StepRef
export interface StepRef {
  phase: PhaseKind;
  kind: PrerequisiteStepKind;
  index: number;
  description: string;
}

// Rust: data::workflow_prerequisites::WorkflowPrerequisite (tag = "type"); GitHubToken → "git_hub_token"
export type WorkflowPrerequisite =
  | { type: 'git_identity' }
  | { type: 'git_remote'; remote: string }
  | { type: 'push_auth'; remote: string }
  | { type: 'push_auth_ssh'; remote: string; host: string }
  | { type: 'push_auth_https'; remote: string; host: string }
  | { type: 'git_hub_token'; host: string }
  | { type: 'container_tool'; name: string }
  | { type: 'pushed_branch' }
  | { type: 'not_detached_head' };

// Rust: data::workflow_prerequisites::PrerequisiteOrigin
export type PrerequisiteOrigin = 'static' | 'assumed_all';
// Rust: data::workflow_prerequisites::PrerequisiteStatus
export type PrerequisiteStatus = 'satisfied' | 'needs_consent' | 'missing' | 'unknown';
// Rust: data::workflow_prerequisites::AcquireScope
export type AcquireScope = 'global' | 'repo';
// Rust: data::workflow_prerequisites::PrerequisitePhase
export type PrerequisitePhase = 'start' | 'before_teardown';

// Rust: data::workflow_prerequisites::Remediation (tag = "type")
export type Remediation =
  | { type: 'replace_rejected_token' }
  | { type: 'use_host_identity'; name: string; email: string }
  | { type: 'use_repo_config_identity'; name: string; email: string }
  | { type: 'enter_identity' }
  | { type: 'set_local_identity'; name: string; email: string; command: string }
  | {
      type: 'mount_ssh_key';
      path: string;
      passphrase_protected: boolean | null;
      mode_ok: boolean;
      remembered: boolean;
      suggested: boolean;
    }
  | { type: 'mount_ssh_dir' }
  | { type: 'use_env_token'; var: string; remembered: boolean }
  | { type: 'paste_token' }
  | { type: 'run_gh_auth_token'; argv: string[]; resolved_path: string; logged_in: boolean | null }
  | { type: 'run_gh_auth_login'; argv: string[]; resolved_path: string }
  | {
      type: 'run_acquire_command';
      label: string;
      argv: string[];
      resolved_path: string | null;
      scope: AcquireScope;
      approved: boolean;
      sha256: string;
    }
  | { type: 'add_dockerfile_line'; file: string; line: string }
  | { type: 'add_overlay_hint'; hint: string }
  | { type: 'continue' }
  | { type: 'abort' };

// Rust: data::workflow_prerequisites::PrerequisiteCheck
export interface PrerequisiteCheck {
  prerequisite: WorkflowPrerequisite;
  status: PrerequisiteStatus;
  explanation: string;
  needed_by: StepRef[];
  remediations: Remediation[];
}

// Rust: data::workflow_prerequisites::PrerequisiteAdvisory
export interface PrerequisiteAdvisory {
  step: StepRef;
  message: string;
}

// Rust: data::workflow_prerequisites::WorkflowPrerequisiteReport
export interface WorkflowPrerequisiteReport {
  phase: PrerequisitePhase;
  origin: PrerequisiteOrigin;
  checks: PrerequisiteCheck[];
  advisories: PrerequisiteAdvisory[];
  notes: string[];
}

// Rust: data::dialog::ReviewBody (tag = "kind", content = "value")
export type ReviewBody =
  | { kind: 'exact'; value: string }
  | { kind: 'prose'; value: string }
  | { kind: 'prerequisites'; value: WorkflowPrerequisiteReport };

/** `(char, String)` tuple: [hotkey, label]. */
export type KeyChoice = [string, string];

// Rust: data::dialog::ReviewRequest
export interface ReviewRequest {
  title: string;
  body: ReviewBody;
  note: string;
  keys: KeyChoice[];
  gated_keys: string[];
  dismiss_label: string | null;
}

// Rust: data::dialog::DialogRequest (tag = "kind", rename_all = snake_case; newtype variants flatten their state)
export type DialogRequest =
  | { kind: 'yes_no'; title: string; body: string }
  | { kind: 'yes_no_cancel'; title: string; body: string }
  | { kind: 'text_input'; title: string; prompt: string; default_text: string | null }
  | { kind: 'secret_input'; title: string; prompt: string }
  | { kind: 'terminal_release'; title: string; terminal_id: TerminalId | null }
  | { kind: 'terminal_restore' }
  | { kind: 'multiline_input'; title: string; prompt: string; default_text: string | null }
  | { kind: 'list_picker'; title: string; items: string[] }
  /** options: [value, label] pairs; answered with Index. */
  | { kind: 'kind_select'; title: string; options: [string, string][] }
  | ({ kind: 'workflow_control_board' } & WorkflowControlBoardState)
  | ({ kind: 'workflow_yolo_countdown' } & WorkflowYoloCountdownState)
  | ({ kind: 'workflow_step_confirm' } & WorkflowStepConfirmState)
  | ({ kind: 'agent_setup' } & AgentSetupState)
  | ({ kind: 'mount_scope' } & MountScopeState)
  | ({ kind: 'agent_auth' } & AgentAuthState)
  | { kind: 'quit_confirm' }
  | { kind: 'close_tab_confirm' }
  | { kind: 'workflow_cancel_confirm' }
  | { kind: 'config_show'; rows: ConfigShowRow[]; selected: number; rejected: ConfigShowRejectedEdit | null }
  /** Not awaited: never answer. */
  | { kind: 'loading'; title: string }
  | { kind: 'custom'; title: string; body: string; keys: KeyChoice[] }
  | ({ kind: 'review' } & ReviewRequest)
  /** Not awaited: never answer. */
  | { kind: 'key_setup_notice'; title: string; body: string; copy_key: string; copy_zshrc_snippet: string };

export type DialogRequestKind = DialogRequest['kind'];
export type DialogRequestOf<K extends DialogRequestKind> = Extract<DialogRequest, { kind: K }>;

// Rust: data::dialog::DialogResponse (tag = "kind", content = "value", rename_all = snake_case)
// `secret` uses data::secret::local_socket_wire (design §3.5: renamed `frontend_wire`) — the plain string value.
export type DialogResponse =
  | { kind: 'interrupted' }
  | { kind: 'yes' }
  | { kind: 'no' }
  | { kind: 'cancel' }
  | { kind: 'text'; value: string }
  | { kind: 'secret'; value: string }
  | { kind: 'index'; value: number }
  /** exactly one Unicode scalar value */
  | { kind: 'char'; value: string }
  | { kind: 'dismissed' }
  | { kind: 'config_edit'; value: ConfigEditRequest };

export type DialogResponseKind = DialogResponse['kind'];

// Rust: data::instance_snapshot::DialogAnswerOutcome (tag = "outcome") — HTTP answer route result
export type DialogAnswerOutcome =
  | { outcome: 'answered' }
  | { outcome: 'already_answered'; by: AnsweredBy }
  | { outcome: 'rejected' };

/* ─────────────────────────────── awman.terminal.v1 ─────────────────────────── */

// Rust: data::terminal::TerminalFrame (tag = "type") — server → client, JSON text messages
export type TerminalFrame =
  /** A refused client frame (`input_not_allowed`, `control_held`, `input_backpressure`, `conflict`, `invalid_request`…); socket stays open. */
  | { type: 'error'; code: string; error: string }
  /** On connect and after `lagged`. `screen` = ANSI that redraws the screen (base64). `has_output` defaults to true when absent. */
  | { type: 'snapshot'; has_output?: boolean; cols: number; rows: number; screen: Base64 }
  | { type: 'output'; bytes: Base64 }
  | { type: 'resize'; cols: number; rows: number }
  | { type: 'exit'; code: number }
  /** This viewer missed output; a `snapshot` follows. */
  | { type: 'lagged' };

export type TerminalFrameType = TerminalFrame['type'];

// Rust: data::terminal::DetachReason
export type TerminalDetachReason = 'user' | 'hangup';

// Rust: data::terminal::TerminalClientFrame (tag = "type") — client → server
export type TerminalClientFrame =
  | { type: 'input'; bytes: Base64 }
  /** Honoured only for the control holder. */
  | { type: 'resize'; cols: number; rows: number }
  /** Request the exclusive input grant. Success is silent; refusal is an `error` frame. */
  | { type: 'take_control' }
  | { type: 'detached'; reason: TerminalDetachReason };

export type TerminalClientFrameType = TerminalClientFrame['type'];

// Rust: data::terminal::TerminalState
export type TerminalState = 'open' | 'exited';

// Rust: data::daemon_state::FeatureName
export type FeatureName = 'squad' | 'worker' | 'engine';

// Rust: data::execution_event::EventSourcePhase
export type EventSourcePhase = 'leader' | 'setup' | 'main' | 'teardown' | 'merge' | 'push';

// Rust: data::execution_event::EventSource
export interface EventSource {
  phase: EventSourcePhase;
  step_name?: string;
  step_index?: number;
  container?: string;
  step_attempt?: number;
}

// Rust: data::terminal::TerminalRecord — `GET /v1/terminals[?owner_id=]` items, `GET /v1/terminals/{id}`
export interface TerminalRecord {
  id: TerminalId;
  /** TerminalOwner as its prefixed id: cmd_… | tab_… | lease_… | run_… */
  owner_id: string;
  feature: FeatureName;
  source?: EventSource;
  agent: string;
  model?: string;
  attempt: number;
  cols: number;
  rows: number;
  state: TerminalState;
  exit_code?: number;
  created_at: Timestamp;
  allow_remote_input: boolean;
}

/* ──────────────────────────────────── HTTP ─────────────────────────────────── */

// Rust: frontend::api::error::ApiError body (with_details flattens extra route fields beside these)
export interface ApiErrorBody {
  error: string;
  code: ApiErrorCode;
}

// Rust: frontend::api::error constructors! table (+ host/attach codes, + design §2.x codes in progress)
export type KnownApiErrorCode =
  | 'host_mismatch'
  | 'origin_not_allowed'
  | 'browser_grant_required'
  | 'browser_grant_invalid'
  | 'unauthorized'
  | 'forbidden_workdir'
  | 'invalid_request'
  | 'forbidden'
  | 'not_found'
  | 'feature_disabled'
  | 'feature_degraded'
  | 'ephemeral_not_allowed'
  | 'config_invalid'
  | 'task_not_found'
  | 'run_not_found'
  | 'grant_not_found'
  | 'task_exists'
  | 'conflict'
  | 'prerequisite_widening_refused'
  | 'method_not_allowed'
  | 'output_not_found'
  | 'request_too_large'
  | 'output_too_large'
  | 'unsupported_media_type'
  | 'unprocessable_request'
  | 'session_not_found'
  | 'no_workflow'
  | 'unavailable'
  | 'invalid_annotation'
  | 'terminal_not_found'
  | 'terminal_gone'
  | 'control_held'
  | 'input_not_allowed'
  | 'input_backpressure'
  | 'protocol_error'
  // design §2.4/§2.7/§3.4 (daemon side in progress)
  | 'rate_limited'
  | 'link_gone'
  | 'link_not_found'
  | 'browser_ticket_invalid'
  | 'dir_outside_context';
export type ApiErrorCode = KnownApiErrorCode | (string & {});

// Rust: frontend::api::routes listener_gate — non-browser `GET /v1/health`
export interface HealthBasic {
  status: 'ok';
}

// design §2.6 (daemon side in progress): browser `GET /v1/health` body (allowed Origin)
export interface BrowserHealth {
  status: 'ok';
  build: string;
  /** `browser` present (=1) only when `browser_links` is true. */
  protocols: { api: number; frontend: number; terminal: number; browser?: number };
  capabilities: string[];
  listener: { name: string; transport: ListenerTransport; features: FeatureName[] };
  engine: { state: 'on' | 'off' | 'draining' | 'degraded' };
  browser_links: boolean;
}

// design §2.4 (daemon side in progress): `POST /v1/browser-links` → 201
export interface BrowserLinkCreated {
  id: BrowserLinkId;
  /** `web-XXXX-XXXX` */
  code: string;
  /** 43 base64url chars; required for every poll; returned once. */
  polling_secret: string;
  interval_ms: number;
  expires_at: Timestamp;
  listener: string;
  origin: string;
  scope: 'engine';
}

// design §2.4 (daemon side in progress): poll request body
export interface BrowserLinkPollRequest {
  polling_secret: string;
}

// design §2.4/§2.5 (daemon side in progress): grant as released to the poller once
export interface BrowserGrantWire {
  id: BrowserGrantId;
  /** `awb_` + 43 base64url chars */
  token: string;
  scope: 'engine';
  origin: string;
  listener: string;
  expires_at: Timestamp;
}

// design §2.4 (daemon side in progress): `POST /v1/browser-links/{id}/poll` → 200 (410 link_gone / 429 rate_limited are ApiErrorBody)
export type BrowserLinkPollResponse =
  | { status: 'pending'; retry_after_ms: number }
  | { status: 'approved'; grant: BrowserGrantWire }
  | { status: 'denied' };

// design §2.7 (daemon side in progress): `POST /v1/browser-tickets` body
export type BrowserTicketRequest =
  | { route: 'frontend'; instance_id: InstanceId }
  | { route: 'terminal'; terminal_id: TerminalId };

// design §2.7 (daemon side in progress): → 201
export interface BrowserTicketResponse {
  /** `awt_<43 base64url>`; one use; 30 s */
  ticket: string;
  expires_in_ms: number;
}

/* /v1/status */

// Rust: data::daemon_state::FeatureState
export type FeatureState = 'off' | 'on' | 'ephemeral' | 'draining' | 'degraded';
// Rust: data::daemon_state::EnabledBy
export type EnabledBy = 'config' | 'ephemeral';

// Rust: command::daemon_api::core::FeatureRecord
export interface FeatureRecord {
  enabled_by: EnabledBy | null;
  reason?: string;
  oldest_queued_age_ms: number | null;
  restarts: number;
  last_error: string | null;
  since_at: Timestamp;
  idle_timeout_ms?: number;
  idle_ms?: number;
  name: string;
  state: FeatureState;
  max_concurrent_jobs?: number;
  running: number;
  queued: number;
  /** engine: `{instances, frontends}` */
  detail?: JsonValue;
}

// Rust: command::daemon_runtime::network::LocalTransport
export type LocalTransport = 'unix_socket' | 'named_pipe';
// Rust: data::config::network::ListenerTransport
export type ListenerTransport = 'tls' | 'loopback_http';

// Rust: command::daemon_runtime::network::LocalListenerRecord
export interface LocalListenerRecord {
  transport: LocalTransport;
  address: string;
}

// Rust: command::daemon_runtime::network::TlsStatus
export interface TlsStatus {
  fingerprint_sha256: string;
  sans: string[];
}

// Rust: command::daemon_runtime::network::ListenerRecord
export interface NetworkListenerRecord {
  name: string;
  enabled: boolean;
  listen: string;
  /** std::net::SocketAddr as "ip:port" / "[v6]:port" */
  bound: string | null;
  transport: ListenerTransport;
  tls: TlsStatus | null;
  error: string | null;
  features: FeatureName[];
  api_keys: number;
  browser_links: boolean;
  allowed_origins: string[];
  browser_grants: number;
}

// Rust: command::daemon_runtime::network::ListenerStatus
export interface ListenerStatus {
  local: LocalListenerRecord | null;
  network: Record<string, NetworkListenerRecord>;
}

// Rust: command::daemon_api::core::StatusResponse — `GET /v1/status`
export interface StatusResponse {
  status: 'ok' | 'no_features';
  version: string;
  /** currently `{api: 1, frontend: 1}` */
  protocol: Record<string, number>;
  capabilities: string[];
  /** keyed by feature name: squad, worker, engine */
  features: Record<string, FeatureRecord>;
  max_concurrent_jobs: number;
  pid: number;
  uptime_ms: number;
  listeners: ListenerStatus;
  agents: string[];
}

/* launch contexts */

// design §3.1/§3.4 (daemon side in progress): data::launch_context::LaunchContextSummary
export interface LaunchContextSummary {
  id: LaunchContextId;
  label: string;
  /** display string of the canonical root */
  root: string;
  is_default: boolean;
  env_state: 'live' | 'stale';
  refreshed_at: Timestamp;
}

// design §3.4 (daemon side in progress): `GET /v1/engine/launch_contexts`
export interface LaunchContextList {
  items: LaunchContextSummary[];
  default_id: LaunchContextId | null;
  next_cursor: string | null;
}

/* catalogue + prompts */

// design §2.9 (daemon side in progress): catalogue flag
export interface CatalogueFlag {
  long: string;
  short: string | null;
  help: string;
  kind: string;
  default: string | null;
  values?: string[];
}

// design §2.9 (daemon side in progress): catalogue positional argument (shape not pinned by the design)
export interface CatalogueArgument {
  name: string;
  help: string;
  required?: boolean;
  values?: string[];
}

// design §2.9 (daemon side in progress): catalogue command node
export interface CatalogueCommand {
  path: string[];
  help: string;
  long_help: string | null;
  aliases: string[];
  flags: CatalogueFlag[];
  arguments: CatalogueArgument[];
  subcommands: CatalogueCommand[];
}

// design §2.9 (daemon side in progress): `GET /v1/engine/catalogue` (ETag; If-None-Match → 304)
export interface Catalogue {
  etag: string;
  commands: CatalogueCommand[];
  /** named UI actions → argv path, e.g. show_config: ["config","show"] */
  actions: Record<string, string[]>;
}

// design §2.9 (daemon side in progress): kinds served by `GET /v1/engine/prompts?kind=`
export type PromptKind = 'squad_task_confirm' | 'quit_confirm' | 'close_tab_confirm' | 'workflow_cancel_confirm' | 'new_tab';

// design §2.9 (daemon side in progress): projection of data::prompt::Choice<D> (no serde in Rust yet)
export interface PromptChoice {
  /** one-char hotkey */
  key: string;
  label: string;
  /** the decision D, as its wire string */
  value: string;
}

// design §2.9 (daemon side in progress): projection of data::prompt::Prompt<D>
// Field name for the dismissal default follows Rust `Prompt::default_on_dismiss`; design text says "default".
export interface PromptView {
  title: string;
  body: string;
  choices: PromptChoice[];
  default_on_dismiss: string | null;
}

/* engine instances */

// Rust: command::daemon_api::engine::CreateInstanceRequest — `POST /v1/engine/instances` body
export interface CreateInstanceRequest {
  annotations?: Annotations;
}

// Rust: command::daemon_api::engine::CreateInstanceResponse — 201
export interface CreateInstanceResponse {
  id: InstanceId;
}

// Rust: data::instance_snapshot::InstanceSummary — `GET /v1/engine/instances` → List<InstanceSummary>
export interface InstanceSummary {
  id: InstanceId;
  primary_frontend_kind: FrontendKindWire | null;
  annotations: Annotations;
  tab_count: number;
  running_jobs: number;
  created_at: Timestamp;
}

// Rust: data::instance_snapshot::InstanceDetail — `GET /v1/engine/instances/{id}` (summary flattened)
export interface InstanceDetail extends InstanceSummary {
  frontends: FrontendSummary[];
  tabs: TabSnapshot[];
}

// Rust: command::daemon_api::engine::PatchAnnotations — PATCH instance / tab body
export interface PatchAnnotations {
  annotations: Annotations;
}

// Rust: command::daemon_api::engine::AnswerRequest — `POST …/dialogs/{dialog_id}/answer` body
export interface AnswerRequest {
  response: DialogResponse;
}

/* squad (`/v1/squad/*`) */

// Rust: data::fs::task_store::TaskStatus (lowercase)
export type SquadTaskStatus = 'active' | 'paused';
// Rust: data::fs::task_store::MountScope (lowercase) — note "gitroot"
export type SquadMountScope = 'cwd' | 'gitroot' | 'directory';
// Rust: data::fs::task_store::RunStatus (snake_case)
export type SquadRunStatus = 'running' | 'not_triggered' | 'workflow_executed' | 'failed' | 'interrupted' | 'canceled';

// Rust: command::daemon_api::squad::TaskResponse(data::fs::task_store::Task) — interval_secs → interval_ms, timestamps millisecond RFC 3339
export interface SquadTask {
  /** data::config::workflows::PrerequisitePolicy — opaque to the web app */
  prerequisites: JsonValue | null;
  id: TaskId;
  name: string;
  description: string;
  repo_scope: string;
  mount_scope: SquadMountScope;
  overlays: string[];
  interval_ms: number;
  status: SquadTaskStatus;
  agent: string | null;
  model: string | null;
  backoff_until: Timestamp | null;
  created_at: Timestamp;
  updated_at: Timestamp;
  last_run_at: Timestamp | null;
  trigger_requested_at: Timestamp | null;
  last_run_status: SquadRunStatus | null;
  unmet_env: string[];
}

// Rust: command::daemon_api::squad::RunResponse(data::fs::task_store::Run)
export interface SquadRun {
  id: RunId;
  task_id: TaskId;
  status: SquadRunStatus;
  workflow_path: string | null;
  workflow_state_path: string | null;
  session_id: WorkerSessionId | null;
  started_at: Timestamp;
  finished_at: Timestamp | null;
  error: string | null;
  reason: string | null;
  unmet_env: string[];
}

// Rust: command::daemon_api::squad::TaskPatch (paused + flattened gateway::UpdateTask) — `PATCH /v1/squad/tasks/{id}`
export interface SquadTaskPatch {
  paused?: boolean;
  description?: string;
  /** multiple of 1000 */
  interval_ms?: number;
  /** null clears to the squad default */
  agent?: string | null;
  model?: string | null;
  overlays?: string[];
  agents_to_models?: Record<string, string[]>;
  prerequisites?: JsonValue;
}

// Rust: command::daemon_api::squad::RunCreated — `POST /v1/squad/tasks/{id}/runs` → 202
export interface SquadRunCreated {
  id: RunId;
}

// Rust: command::daemon_api::squad::RunCanceled — `POST /v1/squad/runs/{id}/cancel`
export interface SquadRunCanceled {
  id: RunId;
  /** "canceling" when it was running, else the run's final status */
  status: 'canceling' | SquadRunStatus;
}

/* ───────────────────────── runtime tag lists (validation) ──────────────────── */

export const FRONTEND_FRAME_TYPES = [
  'hello',
  'worker_queue',
  'open_tab',
  'close_tab',
  'focus_tab',
  'submit',
  'dialog_response',
  'cancel',
  'cancel_countdown',
  'workflow_request',
  'activity',
  'resize',
  'ui_request',
  'quit',
] as const;

export const ENGINE_FRAME_TYPES = [
  'worker_queue',
  'command_bytes',
  'welcome',
  'snapshot',
  'snapshot_chunk',
  'delta',
  'command_output',
  'command_finished',
  'dialog_request',
  'dialog_closed',
  'program_status',
  'notice',
  'ui_ack',
  'detach',
] as const;

export const INSTANCE_DELTA_TYPES = [
  'tab_opened',
  'tab_closed',
  'tab_label',
  'phase',
  'workflow',
  'slot_launched',
  'slot_container_name',
  'slot_exited',
  'slot_stuck',
  'slot_stats',
  'slot_countdown_kind',
  'slot_yolo',
  'slot_acp_update',
  'slot_acp_permission',
  'group_started',
  'group_finished',
  'focused_slot',
  'yolo',
  'yolo_mode',
  'stuck',
  'git_summary',
  'status_line',
  'status_log_reset',
  'agents_dashboard',
  'last_container_summary',
  'annotations',
  'frontend_attached',
  'frontend_detached',
  'squad_health',
  'active_tab_hint',
  'dialog_pending',
  'dialog_resolved',
] as const;

export const DIALOG_REQUEST_TYPES = [
  'yes_no',
  'yes_no_cancel',
  'text_input',
  'secret_input',
  'terminal_release',
  'terminal_restore',
  'multiline_input',
  'list_picker',
  'kind_select',
  'workflow_control_board',
  'workflow_yolo_countdown',
  'workflow_step_confirm',
  'agent_setup',
  'mount_scope',
  'agent_auth',
  'quit_confirm',
  'close_tab_confirm',
  'workflow_cancel_confirm',
  'config_show',
  'loading',
  'custom',
  'review',
  'key_setup_notice',
] as const;

/** Rust: DialogRequest::awaits_answer — these are shown but never answered. */
export const UNANSWERED_DIALOG_TYPES = ['loading', 'key_setup_notice'] as const;

export const DIALOG_RESPONSE_KINDS = [
  'interrupted',
  'yes',
  'no',
  'cancel',
  'text',
  'secret',
  'index',
  'char',
  'dismissed',
  'config_edit',
] as const;

export const UI_REQUEST_NAMES = [
  'open_tab',
  'next_tab',
  'prev_tab',
  'close_tab',
  'set_container_window',
  'open_overview',
  'focus_workflow_window',
  'focus_work',
] as const;

export const DETACH_REASONS = [
  'quit',
  'engine_disabled',
  'instance_ended',
  'replaced',
  'daemon_shutdown',
  'protocol_error',
] as const;

export const TERMINAL_FRAME_TYPES = ['error', 'snapshot', 'output', 'resize', 'exit', 'lagged'] as const;
export const TERMINAL_CLIENT_FRAME_TYPES = ['input', 'resize', 'take_control', 'detached'] as const;

/* Compile-time guarantees that the runtime lists match the unions exactly. */
type SameSet<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
type Assert<T extends true> = T;
export type _TagListChecks = [
  Assert<SameSet<(typeof FRONTEND_FRAME_TYPES)[number], FrontendFrameType>>,
  Assert<SameSet<(typeof ENGINE_FRAME_TYPES)[number], EngineFrameType>>,
  Assert<SameSet<(typeof INSTANCE_DELTA_TYPES)[number], InstanceDeltaType>>,
  Assert<SameSet<(typeof DIALOG_REQUEST_TYPES)[number], DialogRequestKind>>,
  Assert<SameSet<(typeof DIALOG_RESPONSE_KINDS)[number], DialogResponseKind>>,
  Assert<SameSet<(typeof UI_REQUEST_NAMES)[number], UiRequest['request']>>,
  Assert<SameSet<(typeof DETACH_REASONS)[number], DetachReasonWire['reason']>>,
  Assert<SameSet<(typeof TERMINAL_FRAME_TYPES)[number], TerminalFrameType>>,
  Assert<SameSet<(typeof TERMINAL_CLIENT_FRAME_TYPES)[number], TerminalClientFrameType>>,
];
