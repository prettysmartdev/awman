import { vi } from 'vitest';
import frames from '../lib/fixtures/engine-frames.json';
import dialogs from '../lib/fixtures/dialog-requests.json';
import http from '../lib/fixtures/http.json';
import { InstanceStore } from '@/lib/client/instanceStore';
import { Subject } from '@/lib/client/observable';
import type { FrontendConnection, OutboundFrame } from '@/lib/client/frontendSocket';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { Catalogue, DialogRequest, DialogRequestKind, EngineFrame, InstanceSnapshot, LaunchContextList, PromptKind, PromptView, SquadTask } from '@/lib/client/types';

export function snapshot(): InstanceSnapshot {
  const f = structuredClone(frames.find(f => f.type === 'snapshot'));
  if (!f) throw new Error('fixture missing');
  const { seq: _seq, type: _type, ...s } = f;
  return s as unknown as InstanceSnapshot;
}
export function request<K extends DialogRequestKind>(kind: K): Extract<DialogRequest, { kind: K }> {
  return structuredClone(dialogs.find(d => d.kind === kind)) as Extract<DialogRequest, { kind: K }>;
}
export function harness() {
  const store = new InstanceStore(); const s = snapshot();
  s.tabs[0]!.execution_phase = { phase: 'idle' }; s.tabs[0]!.slots = []; s.pending_dialogs = [];
  store.applySnapshot(s);
  const callbacks = new Set<(f: EngineFrame) => void>();
  const state = new Subject<'open' | 'closed' | 'connecting' | 'reconnecting'>('open');
  const role = new Subject<'primary' | 'secondary' | null>('primary');
  const send = vi.fn((_frame: OutboundFrame) => 1); const uiRequest = vi.fn(async () => ({ claimed_by_host: false, claimed_by: null, detail: null }));
  const connection = { store, state, role, id: 'fe_abc', close: vi.fn(), send, uiRequest, onFrame: (cb: (f: EngineFrame) => void) => { callbacks.add(cb); return () => callbacks.delete(cb); } } as unknown as FrontendConnection;
  const contexts: LaunchContextList = { items: [{ id: 'lc_default', label: 'Project', root: '/repo', is_default: true, env_state: 'live', refreshed_at: '' }], default_id: 'lc_default', next_cursor: null };
  const catalogue: Catalogue = { etag: 'one', actions: { show_config: ['config', 'show'], new_squad_task: ['squad', 'add', '--interview'], edit_squad_task: ['squad', 'edit', '--interview'] }, commands: ['ready', 'review'].map(name => ({ path: [name], help: `${name} help`, long_help: null, aliases: [], flags: [{ long: 'watch', short: null, help: 'Watch updates', kind: 'bool', default: null }], arguments: [], subcommands: [] })) };
  const prompt = vi.fn(async (kind: PromptKind): Promise<PromptView> => ({ title: kind, body: `Engine wording for ${kind}`, choices: kind === 'squad_task_confirm' ? [{ key: 'y', label: 'Confirm task action', value: 'dispatch' }, { key: 'n', label: 'Back', value: 'dismiss' }] : kind === 'close_tab_confirm' ? [{ key: 'c', label: 'Quit awman', value: 'quit' }, { key: 't', label: 'Close tab', value: 'close_tab' }] : kind === 'workflow_cancel_confirm' ? [{ key: 'y', label: 'Cancel workflow', value: 'abort' }, { key: 'n', label: 'Keep running', value: 'dismiss' }] : [{ key: 'c', label: 'Quit awman', value: 'quit' }], default_on_dismiss: 'dismiss' }));
  const task = ((http as unknown as { squadTasks?: { items: SquadTask[] } }).squadTasks?.items[0] ?? { id: 'task_one', name: 'Daily', description: 'Review code', status: 'active', unmet_env: [], last_run_status: null }) as SquadTask;
  const client = { catalogue: vi.fn(async () => catalogue), launchContexts: vi.fn(async () => contexts), listInstances: vi.fn(async () => []), createInstance: vi.fn(async () => ({ id: 'ins_new' })), prompt, terminals: vi.fn(async () => []), squad: { tasks: vi.fn(async () => [task]), task: vi.fn(async () => task), runs: vi.fn(async () => [{ id: 'run_one', task_id: task.id, status: 'running', started_at: '', error: null, reason: null }]), patch: vi.fn(async () => task), trigger: vi.fn(async () => ({ id: 'run_new' })), cancelRun: vi.fn(async () => ({ id: 'run_one', status: 'canceling' })), delete: vi.fn(async () => undefined) } };
  return { connection, store, s, state, role, send, uiRequest, client: client as unknown as DaemonClient, api: client, task, contexts, emit: (f: EngineFrame) => { if (f.type === 'delta') store.applyDelta(f); else store.applyFrame(f); for (const cb of callbacks) cb(f); } };
}
