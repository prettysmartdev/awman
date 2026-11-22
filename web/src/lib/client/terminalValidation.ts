// Validate the additive v1 notifications before making them observable.
const object = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);
const text = (v: unknown): v is string => typeof v === 'string';
const nullableText = (v: unknown) => v === null || text(v);
const nullableBool = (v: unknown) => v === null || typeof v === 'boolean';
const member = (v: unknown, values: readonly string[]) => text(v) && values.includes(v);
const safe = (v: unknown) => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const id = (v: unknown, prefix: string) => text(v) && new RegExp(`^${prefix}_[0-9a-f]{32}$`).test(v);
const uuid = (v: unknown) => text(v) && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(v);
const native = (v: unknown) => v === null || (text(v) && new TextEncoder().encode(v).length >= 1 && new TextEncoder().encode(v).length <= 512 && !/[\p{Cc}]/u.test(v));
const support = (v: unknown) => member(v, ['full', 'best_effort', 'none']);
const source = (v: unknown) => member(v, ['native', 'osc7501', 'none']);
const mode = (v: unknown) => member(v, ['default', 'plan', 'read_only', 'custom']);
const access = (v: unknown) => member(v, ['read_write', 'read_only']);
function capability(v: unknown): boolean {
  if (!object(v) || !text(v.agent) || !text(v.runtime) || !nullableText(v.provider_version) || !nullableText(v.image_digest) || !nullableText(v.minimum_version) || !nullableText(v.version_warning) || !member(v.evidence, ['declaration', 'probe', 'observed', 'none']) || typeof v.settings_profiles !== 'boolean') return false;
  const p = v.native_plan, h = v.headless_turns, t = v.terminal_turns, r = v.native_resume, c = v.credential_updates;
  return object(p) && nullableBool(p.interactive) && nullableBool(p.headless) && nullableBool(p.acp) && (p.kind === null || member(p.kind, ['plan', 'read_only', 'custom'])) &&
    object(h) && nullableBool(h.supported) && nullableText(h.reason) && object(t) && nullableBool(t.supported) && (t.status_support === null || support(t.status_support)) && Array.isArray(t.sources) && t.sources.every(s => member(s, ['native', 'osc7501'])) && nullableText(t.reason) &&
    object(r) && nullableBool(r.interactive) && nullableBool(r.headless) && nullableText(r.reason) && object(c) && Array.isArray(c.routes) && c.routes.every(route => object(route) && member(route.credential_kind, ['api_key', 'oauth_access_token', 'github_token', 'provider_managed', 'unknown']) && member(route.delivery, ['env', 'file', 'helper', 'native_store', 'unknown']) && nullableText(route.model_provider) && nullableBool(route.live_update) && nullableBool(route.restart_supported) && member(route.refresh_owner, ['awman', 'provider', 'caller', 'none']) && (route.update_boundary === null || member(route.update_boundary, ['immediate', 'next_request', 'idle'])) && (route.max_apply_delay === null || safe(route.max_apply_delay)) && (route.restart_scope === null || member(route.restart_scope, ['process', 'execution'])) && (route.conversation_continuity === null || member(route.conversation_continuity, ['native_resume', 'new_session_required'])) && nullableText(route.reason));
}
function result(v: unknown): boolean {
  return object(v) && member(v.status, ['completed', 'completed_unverified', 'failed', 'cancelled', 'delivery_unknown']) && support(v.status_support) && member(v.readiness_source, ['native', 'osc7501']) && source(v.completion_source) && typeof v.completion_verified === 'boolean' && native(v.native_turn_id) && nullableText(v.final_message) && member(v.output_status, ['complete', 'incomplete']) && (v.failure === null || (object(v.failure) && text(v.failure.code) && text(v.failure.message)));
}
function commandResult(v: unknown): boolean {
  if (!object(v) || !(v.exit_code === null || (typeof v.exit_code === 'number' && Number.isInteger(v.exit_code)))) return false;
  const a = v.automation;
  return a === undefined || (object(a) && id(a.execution_id, 'exe') && native(a.native_session_id) && (a.last_turn_id === null || id(a.last_turn_id, 'turn')) && mode(a.native_start_mode) && access(a.workspace_access) && support(a.status_support) && typeof a.completion_verified === 'boolean' && member(a.output_status, ['complete', 'incomplete']) && member(a.cleanup_status, ['complete', 'pending', 'failed']));
}
export function validateAutomationFrame(v: unknown): void {
  if (!object(v)) throw new Error('invalid automation frame');
  let valid = false;
  switch (v.type) {
    case 'automation_state':
      valid = id(v.execution_id, 'exe') && text(v.provider) && nullableText(v.provider_version) && native(v.native_session_id) && member(v.automation_mode, ['auto', 'require_full']) && member(v.minimum_status_support, ['full', 'best_effort']) && member(v.state, ['starting', 'unknown', 'ready', 'submitting', 'working', 'blocked_auth', 'blocked_permission', 'blocked_input', 'credential_updating', 'restarting', 'stopping', 'ended']) && safe(v.revision) && (v.active_turn_id === null || id(v.active_turn_id, 'turn')) && mode(v.native_start_mode) && access(v.workspace_access) && support(v.status_support) && source(v.status_source) && nullableText(v.status_reason) && capability(v.capabilities);
      break;
    case 'turn_accepted': valid = uuid(v.request_id) && id(v.execution_id, 'exe') && id(v.turn_id, 'turn') && support(v.status_support) && source(v.status_source); break;
    case 'turn_result': valid = uuid(v.request_id) && id(v.execution_id, 'exe') && id(v.turn_id, 'turn') && result(v.result); break;
    case 'execution_closed': valid = uuid(v.request_id) && id(v.execution_id, 'exe') && commandResult(v.result); break;
    case 'execution_replaced': valid = uuid(v.request_id) && id(v.previous_execution_id, 'exe') && id(v.execution_id, 'exe') && safe(v.credential_generation); break;
    case 'credential_update_state': {
      const s = v.snapshot;
      valid = object(s) && id(s.execution_id, 'exe') && safe(s.revision) && Array.isArray(s.generations) && s.generations.length <= 32 && s.generations.every(g => object(g) && safe(g.credential_generation) && (g.pending_generation === null || safe(g.pending_generation)) && member(g.generation_state, ['confirmed', 'unknown']) && member(g.refresh_owner, ['awman', 'provider', 'caller', 'none']) && (g.status === null || member(g.status, ['pending', 'applying', 'restarting', 'applied', 'failed'])));
      break;
    }
    case 'operation_error': valid = uuid(v.request_id) && text(v.code) && text(v.error); break;
  }
  if (!valid) throw new Error('invalid automation frame');
}
