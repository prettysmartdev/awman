import { describe, expect, it } from 'vitest';
import { InstanceStore, MAX_LINE_CHARS, OUTPUT_CHAR_LIMIT, OUTPUT_LIMIT, stripAnsi } from '@/lib/client/instanceStore';
import type { DialogRequest, EngineFrame, InstanceSnapshot } from '@/lib/client/types';
import { fixture } from './helpers';

const frames = fixture<(EngineFrame & { seq: number })[]>('engine-frames.json');
const { type: _t, seq: _s, ...body } = frames.find(f => f.type === 'snapshot')!;
const SNAPSHOT = body as unknown as InstanceSnapshot;
const TAB = SNAPSHOT.tabs[0]!.tab_id;
const YES_NO: DialogRequest = { kind: 'yes_no', title: 'Continue?', body: 'Proceed with the step.' };

function storeWithSnapshot() {
  const store = new InstanceStore();
  store.applySnapshot({ ...SNAPSHOT, pending_dialogs: [] });
  return store;
}

describe('InstanceStore', () => {
  it('first answer wins: a closed dialog is removed and remembered, a late duplicate request is ignored', () => {
    const store = storeWithSnapshot();
    store.applyFrame({ type: 'dialog_request', dialog_id: 'dlg_1', tab_id: TAB, request: YES_NO });
    store.applyDelta({ delta: 'dialog_pending', value: { dialog_id: 'dlg_1', tab_id: TAB, request: YES_NO, raised_at: '2026-11-09T00:00:00Z' } });
    expect(store.get().dialogs).toHaveLength(1);
    store.applyFrame({ type: 'dialog_closed', dialog_id: 'dlg_1', answered_by: { by: 'frontend', frontend_id: 'frn_other' } });
    expect(store.get().dialogs).toEqual([]);
    expect(store.get().closedDialogs.get('dlg_1')).toEqual({ by: 'frontend', frontend_id: 'frn_other' });
    store.applyFrame({ type: 'dialog_request', dialog_id: 'dlg_1', tab_id: TAB, request: YES_NO });
    expect(store.get().dialogs).toEqual([]);
  });

  it('a snapshot restores pending dialogs from the engine (resync after reconnect)', () => {
    const store = new InstanceStore();
    store.applySnapshot({ ...SNAPSHOT, pending_dialogs: [{ dialog_id: 'dlg_9', tab_id: TAB, request: YES_NO, raised_at: '2026-11-09T00:00:00Z' }] });
    expect(store.get().dialogs.map(d => d.dialogId)).toEqual(['dlg_9']);
  });

  it('bounds output per tab and drops output for closed tabs', () => {
    const store = storeWithSnapshot();
    const initial = Array.from({ length: OUTPUT_LIMIT }, (_, i) => `line ${i}`).join('\n');
    store.applyFrame({ type: 'command_output', tab_id: TAB, stream: 'stdout', level: 'info', text: initial });
    for (let i = OUTPUT_LIMIT; i < OUTPUT_LIMIT + 10; i++) store.applyFrame({ type: 'command_output', tab_id: TAB, stream: 'stdout', level: 'info', text: `line ${i}` });
    const out = store.get().outputs.get(TAB)!;
    expect(out).toHaveLength(OUTPUT_LIMIT);
    expect(out[0]).toMatchObject({ text: 'line 10' });
    expect(out[out.length - 1]).toMatchObject({ text: `line ${OUTPUT_LIMIT + 9}` });
    store.applyDelta({ delta: 'tab_closed', value: { tab_id: TAB } });
    expect(store.get().outputs.has(TAB)).toBe(false);
  });

  it('notifies subscribers with a new immutable view on every change', () => {
    const store = storeWithSnapshot();
    const seen: unknown[] = [];
    const off = store.subscribe(() => seen.push(store.get()));
    const before = store.get();
    store.notice('warning', 'x', 'client');
    expect(seen).toHaveLength(1);
    expect(store.get()).not.toBe(before);
    off();
    store.notice('info', 'y', 'client');
    expect(seen).toHaveLength(1);
  });

  it('detach marks the instance ended', () => {
    const store = storeWithSnapshot();
    store.applyFrame({ type: 'detach', reason: 'daemon_shutdown' });
    expect(store.get().ended).toEqual({ reason: 'daemon_shutdown' });
  });
});

describe('InstanceStore output assembly (review #7, #8)', () => {
  const bytes = (...values: number[]) => new Uint8Array(values);
  const enc = (text: string) => new TextEncoder().encode(text);
  const lines = (store: InstanceStore, tab = TAB) => (store.get().outputs.get(tab) ?? []).map(e => (e.kind === 'text' ? e.text : '<finished>'));

  it('joins UTF-8 split across CommandBytes frames', () => {
    const store = storeWithSnapshot();
    store.appendBytes(TAB, 'stdout', bytes(0xe2, 0x82));
    store.appendBytes(TAB, 'stdout', bytes(0xac, 0x58));
    expect(lines(store)).toEqual(['€X']);
  });

  it('strips ANSI sequences split across frames and keeps newline-less chunks on one line', () => {
    const store = storeWithSnapshot();
    store.appendBytes(TAB, 'stdout', enc('ab\x1b[3'));
    store.appendBytes(TAB, 'stdout', enc('1mcd\x1b]0;title\x07'));
    store.appendBytes(TAB, 'stdout', enc('ef\x1b[0m\r\nnext'));
    expect(lines(store)).toEqual(['abcdef', 'next']);
    expect(store.get().outputs.get(TAB)!.at(-1)).toMatchObject({ open: true });
  });

  it('keeps stdout and stderr decoders apart and flushes at the command boundary', () => {
    const store = storeWithSnapshot();
    store.appendBytes(TAB, 'stdout', enc('out '));
    store.appendBytes(TAB, 'stderr', bytes(0xe2, 0x82));
    store.appendBytes(TAB, 'stdout', enc('more'));
    store.appendBytes(TAB, 'stderr', bytes(0xac, 0x0a));
    expect(lines(store)).toEqual(['out more', '€']);
    store.appendBytes(TAB, 'stdout', bytes(0x41, 0xe2));
    store.applyFrame({ type: 'command_finished', tab_id: TAB, result: { exit_code: 0, outcome: null, error: null } });
    expect(lines(store).slice(-2)).toEqual(['A�', '<finished>']);
    expect(store.get().outputs.get(TAB)!.some(e => e.kind === 'text' && e.open)).toBe(false);
    store.appendBytes(TAB, 'stdout', enc('fresh'));
    expect(lines(store).at(-1)).toBe('fresh');
  });

  it('bounds lines, not frames: one frame with many lines keeps only the newest OUTPUT_LIMIT', () => {
    const store = storeWithSnapshot();
    const big = Array.from({ length: OUTPUT_LIMIT + 20_000 }, (_, i) => `l${i}`).join('\n');
    store.applyFrame({ type: 'command_output', tab_id: TAB, stream: 'stdout', level: 'info', text: big });
    store.appendBytes(TAB, 'stdout', enc(`${big}\n`));
    const out = lines(store);
    expect(out).toHaveLength(OUTPUT_LIMIT);
    expect(out.at(-1)).toBe(`l${OUTPUT_LIMIT + 19_999}`);
  });

  it('bounds retained characters per tab and truncates huge lines', () => {
    const store = storeWithSnapshot();
    const line = 'x'.repeat(MAX_LINE_CHARS * 2);
    for (let i = 0; i < 200; i++) store.applyFrame({ type: 'command_output', tab_id: TAB, stream: 'stdout', level: 'info', text: line });
    const out = store.get().outputs.get(TAB)!;
    const chars = out.reduce((sum, e) => sum + (e.kind === 'text' ? e.text.length : 0), 0);
    expect(chars).toBeLessThanOrEqual(OUTPUT_CHAR_LIMIT);
    expect(out.every(e => e.kind !== 'text' || e.text.length <= MAX_LINE_CHARS)).toBe(true);
    expect(out.length).toBeLessThan(200);
  });

  it('discards output for tabs the snapshot does not contain', () => {
    const store = storeWithSnapshot();
    store.applyFrame({ type: 'command_output', tab_id: 'tab_unknown', stream: 'stdout', level: 'info', text: 'x' });
    store.appendBytes('tab_unknown', 'stdout', enc('y'));
    expect(store.get().outputs.has('tab_unknown')).toBe(false);
  });

  it('stripAnsi holds back only an unterminated trailing escape', () => {
    expect(stripAnsi('a\x1b[31mb\x1b[')).toEqual(['ab', '\x1b[']);
    expect(stripAnsi('plain')).toEqual(['plain', '']);
  });
});
