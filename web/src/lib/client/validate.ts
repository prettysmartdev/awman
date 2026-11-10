// Runtime validation of inbound awman.frontend.v1 frames (design §5.4).
//
// TypeScript types describe the wire, but the daemon is a separate program
// that may be newer, older or buggy. A frame that does not match what the
// reducers and screens dereference must never be installed as the
// authoritative snapshot: the connection treats it as a protocol error,
// closes, and resynchronizes from a fresh ticket + full Snapshot.
//
// The checks are structural (discriminators, ids, and the fields the client
// reads to update state), not a full schema: presentation-only nested values
// such as workflow views or ACP updates are only required to be objects.
import {
  DIALOG_REQUEST_TYPES,
  ENGINE_FRAME_TYPES,
  INSTANCE_DELTA_TYPES,
  type EngineFrame,
  type InstanceDelta,
  type InstanceSnapshot,
} from './types';

type Obj = Record<string, unknown>;

const isObj = (v: unknown): v is Obj => typeof v === 'object' && v !== null && !Array.isArray(v);
const isStr = (v: unknown): v is string => typeof v === 'string';
const isBool = (v: unknown): v is boolean => typeof v === 'boolean';
const isInt = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v);
const isNum = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
const nullable = (check: (v: unknown) => boolean) => (v: unknown) => v === null || check(v);
const oneOf = (values: readonly string[]) => (v: unknown) => isStr(v) && values.includes(v);
const isArrayOf = (check: (v: unknown) => boolean) => (v: unknown) => Array.isArray(v) && v.every(check);
const isStringMap = (v: unknown) => isObj(v) && Object.values(v).every(isStr);

const LEVELS = ['info', 'warning', 'error', 'success'];
const STREAMS = ['stdout', 'stderr'];
const ROLES = ['primary', 'secondary'];
const FRONTEND_KINDS = ['tui', 'cli', 'web'];
const COUNTDOWN_KINDS = ['stuck_step', 'failure_retry', 'agent_done', 'agent_blocked', 'agent_error'];
const SQUAD_HEALTH = ['unknown', 'running', 'stopped', 'unreachable'];
const DETACH_REASONS = ['quit', 'engine_disabled', 'instance_ended', 'replaced', 'daemon_shutdown', 'protocol_error'];

function isPhase(v: unknown): boolean {
  if (!isObj(v)) return false;
  switch (v.phase) {
    case 'idle': return true;
    case 'running': return isStr(v.command);
    case 'done': return isStr(v.command) && isInt(v.exit_code);
    case 'error': return isStr(v.command) && isStr(v.message);
    default: return false;
  }
}

const isYolo = (v: unknown) => isObj(v) && isStr(v.step_name) && isNum(v.remaining_secs) && oneOf(COUNTDOWN_KINDS)(v.kind);
const isLabel = (v: unknown) => isObj(v) && isStr(v.text) && isStr(v.color);
const isStatusLine = (v: unknown) => isObj(v) && isStr(v.text);
const isDialogRequest = (v: unknown) => isObj(v) && oneOf(DIALOG_REQUEST_TYPES)(v.kind);
const isAcp = (v: unknown) => isObj(v) && Array.isArray(v.history) && (v.pending_permission === null || isObj(v.pending_permission));

function isSlot(v: unknown): boolean {
  return isObj(v)
    && isStr(v.slot_id) && isStr(v.step_name) && isStr(v.agent_display_name) && isStr(v.container_name)
    && nullable(isStr)(v.terminal_id) && nullable(isAcp)(v.acp) && nullable(isObj)(v.stats)
    && nullable(isObj)(v.program_status) && isBool(v.stuck) && oneOf(COUNTDOWN_KINDS)(v.countdown_kind)
    && nullable(isYolo)(v.yolo) && isBool(v.sandboxed) && nullable(isStr)(v.phase_step_title) && isBool(v.dormant);
}

export function isTabSnapshot(v: unknown): boolean {
  return isObj(v)
    && isStr(v.tab_id) && isObj(v.session) && oneOf(['normal', 'squad'])(v.kind) && isLabel(v.label)
    && isPhase(v.execution_phase) && nullable(isObj)(v.current_workflow) && nullable(isObj)(v.current_container)
    && nullable(isObj)(v.workflow) && nullable(isStr)(v.workflow_invocation_id)
    && nullable(isStr)(v.context_path) && nullable(isStr)(v.worktree_path)
    && isArrayOf(isSlot)(v.slots) && nullable(isStr)(v.focused_slot) && nullable(isYolo)(v.yolo)
    && isBool(v.yolo_mode) && isBool(v.stuck) && nullable(isObj)(v.git_summary)
    && isArrayOf(isStatusLine)(v.status_log) && nullable(isObj)(v.agents_dashboard)
    && nullable(isObj)(v.last_container_summary) && isStringMap(v.annotations);
}

const isFrontendSummary = (v: unknown) => isObj(v) && isStr(v.frontend_id) && oneOf(FRONTEND_KINDS)(v.kind) && oneOf(ROLES)(v.role) && isBool(v.reconnecting);
const isPendingDialog = (v: unknown) => isObj(v) && isStr(v.dialog_id) && isStr(v.tab_id) && isDialogRequest(v.request);

export function isInstanceSnapshot(v: unknown): v is InstanceSnapshot {
  return isObj(v)
    && isStr(v.instance_id) && isStringMap(v.annotations) && nullable(oneOf(FRONTEND_KINDS))(v.primary_frontend_kind)
    && isArrayOf(isFrontendSummary)(v.frontends) && isArrayOf(isTabSnapshot)(v.tabs)
    && nullable(isStr)(v.active_tab_hint) && oneOf(SQUAD_HEALTH)(v.squad_health)
    && isArrayOf(isPendingDialog)(v.pending_dialogs) && isInt(v.running_jobs) && v.running_jobs >= 0;
}

function isAnnotationTarget(v: unknown): boolean {
  if (!isObj(v)) return false;
  switch (v.target) {
    case 'instance': return true;
    case 'tab': return isStr(v.tab_id);
    case 'terminal': return isStr(v.terminal_id);
    case 'frontend': return isStr(v.frontend_id);
    default: return false;
  }
}

type ValueCheck = (value: Obj) => boolean;
const tab = (rest: ValueCheck = () => true): ValueCheck => v => isStr(v.tab_id) && rest(v);
const slot = (rest: ValueCheck = () => true): ValueCheck => v => isStr(v.tab_id) && isStr(v.slot_id) && rest(v);

// Object-valued deltas; `squad_health` (a bare string) is handled separately.
const DELTA_VALUES: Record<Exclude<InstanceDelta['delta'], 'squad_health'>, ValueCheck> = {
  tab_opened: isTabSnapshot,
  tab_closed: tab(),
  tab_label: tab(v => isLabel(v.label)),
  phase: tab(v => isPhase(v.phase) && nullable(isObj)(v.current_workflow) && nullable(isObj)(v.current_container)),
  workflow: tab(v => nullable(isObj)(v.view) && nullable(isStr)(v.invocation_id) && nullable(isStr)(v.context_path) && nullable(isStr)(v.worktree_path)),
  slot_launched: tab(v => isSlot(v.slot)),
  slot_container_name: slot(v => isStr(v.container_name)),
  slot_exited: slot(v => nullable(isInt)(v.exit_code)),
  slot_stuck: slot(v => isBool(v.stuck)),
  slot_stats: slot(v => isObj(v.stats)),
  slot_countdown_kind: slot(v => oneOf(COUNTDOWN_KINDS)(v.kind)),
  slot_yolo: slot(v => nullable(isYolo)(v.state)),
  slot_acp_update: slot(v => isObj(v.update)),
  slot_acp_permission: slot(v => nullable(isObj)(v.request)),
  group_started: tab(),
  group_finished: tab(),
  focused_slot: tab(v => nullable(isStr)(v.slot_id)),
  yolo: tab(v => nullable(isYolo)(v.state)),
  yolo_mode: tab(v => isBool(v.on)),
  stuck: tab(v => isBool(v.stuck)),
  git_summary: tab(v => nullable(isObj)(v.summary)),
  status_line: tab(v => isStatusLine(v.line)),
  status_log_reset: tab(),
  agents_dashboard: tab(v => nullable(isObj)(v.data)),
  last_container_summary: tab(v => nullable(isObj)(v.summary)),
  annotations: v => isAnnotationTarget(v.target) && isStringMap(v.annotations),
  frontend_attached: isFrontendSummary,
  frontend_detached: v => isStr(v.frontend_id),
  active_tab_hint: tab(),
  dialog_pending: isPendingDialog,
  dialog_resolved: v => isStr(v.dialog_id),
};

export function isInstanceDelta(v: unknown): v is InstanceDelta {
  if (!isObj(v) || !oneOf(INSTANCE_DELTA_TYPES)(v.delta)) return false;
  if (v.delta === 'squad_health') return oneOf(SQUAD_HEALTH)(v.value);
  const check = DELTA_VALUES[v.delta as keyof typeof DELTA_VALUES];
  return isObj(v.value) && check(v.value);
}

const isAnsweredBy = (v: unknown) => isObj(v) && (v.by === 'api' || v.by === 'engine' || (v.by === 'frontend' && isStr(v.frontend_id)));
const isCommandResult = (v: unknown) => isObj(v) && isInt(v.exit_code) && (v.error === null || (isObj(v.error) && isStr(v.error.code) && isStr(v.error.message)));

/**
 * Validates one parsed envelope (seq already checked). Snapshot frames carry
 * the snapshot inline; snapshot_chunk bodies are validated after reassembly.
 */
export function isEngineFrame(v: Obj): v is Obj & EngineFrame {
  if (!oneOf(ENGINE_FRAME_TYPES)(v.type)) return false;
  switch (v.type as EngineFrame['type']) {
    case 'welcome':
      return isStr(v.frontend_id) && v.frontend_id.length > 0 && isStr(v.instance_id) && isStr(v.protocol) && isStr(v.build) && oneOf(ROLES)(v.role);
    case 'snapshot': {
      const { type: _type, seq: _seq, ...snapshot } = v;
      return isInstanceSnapshot(snapshot);
    }
    case 'snapshot_chunk':
      return isInt(v.chunk) && isInt(v.of) && isStr(v.bytes);
    case 'delta': {
      const { type: _type, seq: _seq, ...delta } = v;
      return isInstanceDelta(delta);
    }
    case 'command_bytes':
      return isStr(v.tab_id) && oneOf(STREAMS)(v.stream) && isStr(v.bytes);
    case 'command_output':
      return isStr(v.tab_id) && oneOf(STREAMS)(v.stream) && oneOf(LEVELS)(v.level) && isStr(v.text);
    case 'command_finished':
      return isStr(v.tab_id) && isCommandResult(v.result);
    case 'dialog_request':
      return isStr(v.dialog_id) && isStr(v.tab_id) && isDialogRequest(v.request);
    case 'dialog_closed':
      return isStr(v.dialog_id) && isAnsweredBy(v.answered_by);
    case 'program_status':
      return isStr(v.tab_id) && isStr(v.slot_id) && nullable(isObj)(v.status);
    case 'notice':
      return oneOf(LEVELS)(v.level) && isStr(v.text);
    case 'ui_ack':
      return isInt(v.request_seq) && isObj(v.outcome);
    case 'detach':
      return oneOf(DETACH_REASONS)(v.reason) && (v.reason !== 'protocol_error' || isStr(v.detail));
  }
}
