import { describe, expect, it } from 'vitest';
import { ACP_HISTORY_LIMIT, STATUS_LOG_LIMIT, applyDelta, applyDeltas } from '@/lib/client/reducers';
import { INSTANCE_DELTA_TYPES, type EngineFrame, type InstanceDelta, type InstanceSnapshot, type TabSnapshot } from '@/lib/client/types';
import { fixture } from './helpers';

const frames = fixture<(EngineFrame & { seq: number })[]>('engine-frames.json');
const deltas = fixture<InstanceDelta[]>('instance-deltas.json');
const snapshotFrame = frames.find(f => f.type === 'snapshot') as EngineFrame & { seq: number };
const { type: _t, seq: _s, ...rest } = snapshotFrame;
const SNAPSHOT = rest as unknown as InstanceSnapshot;

function deepFreeze<T>(value: T): T {
  if (value && typeof value === 'object') {
    Object.freeze(value);
    for (const child of Object.values(value)) deepFreeze(child);
  }
  return value;
}

function deltaOf<K extends InstanceDelta['delta']>(kind: K): Extract<InstanceDelta, { delta: K }> {
  const found = deltas.find(d => d.delta === kind);
  if (!found) throw new Error(`fixture lacks ${kind}`);
  return found as Extract<InstanceDelta, { delta: K }>;
}

const tab = (snapshot: InstanceSnapshot, id: string): TabSnapshot | undefined => snapshot.tabs.find(t => t.tab_id === id);

describe('InstanceDelta reducers', () => {
  it('the fixtures cover every delta kind', () => {
    expect(new Set(deltas.map(d => d.delta))).toEqual(new Set(INSTANCE_DELTA_TYPES));
  });

  it('applies the whole fixture sequence immutably (frozen input never mutated)', () => {
    const frozen = deepFreeze(structuredClone(SNAPSHOT));
    const before = JSON.stringify(frozen);
    const after = applyDeltas(frozen, deltas);
    expect(JSON.stringify(frozen)).toBe(before);
    expect(after).not.toBe(frozen);
  });

  it('every delta kind is handled (each one either changes state or is an explicit no-op)', () => {
    let state = deepFreeze(structuredClone(SNAPSHOT));
    for (const delta of deltas) {
      const next = applyDelta(state, delta);
      if (delta.delta === 'group_started') expect(next).toBe(state);
      state = next;
    }
  });

  it('tab lifecycle: opened, labelled, phase, closed', () => {
    const opened = deltaOf('tab_opened');
    let state = applyDelta(SNAPSHOT, opened);
    expect(tab(state, opened.value.tab_id)).toEqual(opened.value);
    const label = { delta: 'tab_label', value: { tab_id: opened.value.tab_id, label: { ...opened.value.label, text: 'renamed' } } } as InstanceDelta;
    state = applyDelta(state, label);
    expect(tab(state, opened.value.tab_id)?.label).toMatchObject({ text: 'renamed' });
    state = applyDelta(state, { delta: 'phase', value: { tab_id: opened.value.tab_id, phase: { phase: 'running', command: 'ready' }, current_workflow: null, current_container: null } });
    expect(tab(state, opened.value.tab_id)?.execution_phase).toEqual({ phase: 'running', command: 'ready' });
    state = applyDelta(state, { delta: 'active_tab_hint', value: { tab_id: opened.value.tab_id } });
    state = applyDelta(state, { delta: 'tab_closed', value: { tab_id: opened.value.tab_id } });
    expect(tab(state, opened.value.tab_id)).toBeUndefined();
    expect(state.active_tab_hint).toBeNull();
  });

  it('slot lifecycle: launched, renamed, stuck, exited moves focus to the last remaining slot', () => {
    const base = SNAPSHOT.tabs[0]!;
    const slotA = { ...base.slots[0]!, slot_id: 'slot_a' };
    const slotB = { ...base.slots[0]!, slot_id: 'slot_b' };
    let state = applyDelta(SNAPSHOT, { delta: 'slot_launched', value: { tab_id: base.tab_id, slot: slotA } });
    state = applyDelta(state, { delta: 'slot_launched', value: { tab_id: base.tab_id, slot: slotB } });
    state = applyDelta(state, { delta: 'focused_slot', value: { tab_id: base.tab_id, slot_id: 'slot_b' } });
    state = applyDelta(state, { delta: 'slot_container_name', value: { tab_id: base.tab_id, slot_id: 'slot_a', container_name: 'c-1' } });
    state = applyDelta(state, { delta: 'slot_stuck', value: { tab_id: base.tab_id, slot_id: 'slot_a', stuck: true } });
    expect(tab(state, base.tab_id)?.slots.find(s => s.slot_id === 'slot_a')).toMatchObject({ container_name: 'c-1', stuck: true });
    state = applyDelta(state, { delta: 'slot_exited', value: { tab_id: base.tab_id, slot_id: 'slot_b', exit_code: 0 } });
    const after = tab(state, base.tab_id)!;
    expect(after.slots.some(s => s.slot_id === 'slot_b')).toBe(false);
    expect(after.focused_slot).toBe(after.slots[after.slots.length - 1]?.slot_id ?? null);
  });

  it('group_finished clears dormant flags', () => {
    const base = SNAPSHOT.tabs[0]!;
    const dormant = { ...base.slots[0]!, slot_id: 'slot_d', dormant: true };
    let state = applyDelta(SNAPSHOT, { delta: 'slot_launched', value: { tab_id: base.tab_id, slot: dormant } });
    state = applyDelta(state, { delta: 'group_finished', value: { tab_id: base.tab_id } });
    expect(tab(state, base.tab_id)?.slots.every(s => !s.dormant)).toBe(true);
  });

  it('status log appends (bounded) and resets', () => {
    const base = SNAPSHOT.tabs[0]!;
    let state = SNAPSHOT;
    const line = deltaOf('status_line').value.line;
    for (let i = 0; i < STATUS_LOG_LIMIT + 5; i++) state = applyDelta(state, { delta: 'status_line', value: { tab_id: base.tab_id, line } });
    expect(tab(state, base.tab_id)?.status_log).toHaveLength(STATUS_LOG_LIMIT);
    state = applyDelta(state, { delta: 'status_log_reset', value: { tab_id: base.tab_id } });
    expect(tab(state, base.tab_id)?.status_log).toEqual([]);
  });

  it('ACP history is bounded like ACP_HISTORY_LIMIT', () => {
    const update = deltaOf('slot_acp_update');
    const base = SNAPSHOT.tabs.find(t => t.tab_id === update.value.tab_id) ?? SNAPSHOT.tabs[0]!;
    let state = applyDelta(SNAPSHOT, { delta: 'slot_launched', value: { tab_id: base.tab_id, slot: { ...base.slots[0]!, slot_id: 'slot_acp', acp: null } } });
    for (let i = 0; i < ACP_HISTORY_LIMIT + 3; i++) {
      state = applyDelta(state, { delta: 'slot_acp_update', value: { tab_id: base.tab_id, slot_id: 'slot_acp', update: update.value.update } });
    }
    expect(tab(state, base.tab_id)?.slots.find(s => s.slot_id === 'slot_acp')?.acp?.history).toHaveLength(ACP_HISTORY_LIMIT);
  });

  it('dialogs, frontends, annotations and squad health', () => {
    const pending = deltaOf('dialog_pending');
    let state = applyDelta(SNAPSHOT, pending);
    expect(state.pending_dialogs.some(d => d.dialog_id === pending.value.dialog_id)).toBe(true);
    state = applyDelta(state, { delta: 'dialog_resolved', value: { dialog_id: pending.value.dialog_id } });
    expect(state.pending_dialogs.some(d => d.dialog_id === pending.value.dialog_id)).toBe(false);
    const attached = deltaOf('frontend_attached');
    state = applyDelta(state, attached);
    expect(state.frontends.some(f => f.frontend_id === attached.value.frontend_id)).toBe(true);
    state = applyDelta(state, { delta: 'frontend_detached', value: { frontend_id: attached.value.frontend_id, reason: 'closed' } });
    expect(state.frontends.some(f => f.frontend_id === attached.value.frontend_id)).toBe(false);
    state = applyDelta(state, { delta: 'annotations', value: { target: { target: 'instance' }, annotations: { a: 'b' } } });
    expect(state.annotations).toEqual({ a: 'b' });
    state = applyDelta(state, { delta: 'squad_health', value: 'unreachable' });
    expect(state.squad_health).toBe('unreachable');
  });

  it('ignores deltas for unknown tabs and slots (same object back)', () => {
    expect(applyDelta(SNAPSHOT, { delta: 'stuck', value: { tab_id: 'tab_missing', stuck: true } })).toBe(SNAPSHOT);
    expect(applyDelta(SNAPSHOT, { delta: 'slot_stuck', value: { tab_id: SNAPSHOT.tabs[0]!.tab_id, slot_id: 'nope', stuck: true } })).toBe(SNAPSHOT);
    expect(applyDelta(SNAPSHOT, { delta: 'annotations', value: { target: { target: 'terminal', terminal_id: 'term_x' }, annotations: {} } })).toBe(SNAPSHOT);
  });
});

describe('hasRunningWork (review #3)', () => {
  it('derives live work from tab phases and running containers', async () => {
    const { hasRunningWork } = await import('@/lib/client/reducers'); const { isInstanceDelta, isInstanceSnapshot } = await import('@/lib/client/validate');
    expect(isInstanceSnapshot(SNAPSHOT)).toBe(true);
    for (const delta of deltas) expect([delta.delta, isInstanceDelta(delta)]).toEqual([delta.delta, true]);
    const base = JSON.parse(JSON.stringify(SNAPSHOT)) as InstanceSnapshot;
    const idle = { ...base, tabs: base.tabs.map(t => ({ ...t, execution_phase: { phase: 'idle' as const }, slots: [] })) };
    expect(hasRunningWork(null)).toBe(false);
    expect(hasRunningWork(idle)).toBe(false);
    expect(hasRunningWork({ ...idle, tabs: [{ ...idle.tabs[0]!, execution_phase: { phase: 'running', command: 'x' } }] })).toBe(true);
    expect(hasRunningWork({ ...idle, tabs: [{ ...idle.tabs[0]!, slots: base.tabs[0]!.slots }] })).toBe(base.tabs[0]!.slots.length > 0);
  });
});
