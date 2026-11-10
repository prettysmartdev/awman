// @vitest-environment jsdom
// Review #6: the yolo countdown overlay is derived from live Yolo/SlotYolo
// state (as the TUI does), follows the focused/maximized slot during a
// parallel group, and its controls never invent a dialog answer.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react';
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: () => <div data-key-context="terminal" tabIndex={0}>Contract terminal</div> }));
import { Workspace } from '@/components/Workspace';
import { liveCountdownState } from '@/lib/client/countdown';
import type { SlotSnapshot, TabSnapshot, YoloState } from '@/lib/client/types';
import { harness, request, snapshot } from './helpers';
afterEach(() => { cleanup(); sessionStorage.clear(); });

const yolo = (step_name: string, remaining_secs = 30): YoloState => ({ step_name, remaining_secs, kind: 'stuck_step' });

function mount(tabPatch: (tab: TabSnapshot) => TabSnapshot = t => t) {
  const h = harness(); const original = snapshot().tabs[0]!;
  const tab = tabPatch({ ...original, workflow: { steps: [], current_step: 'implement', max_concurrent: 2, reported: {} } as unknown as TabSnapshot['workflow'], execution_phase: { phase: 'running', command: 'exec workflow' }, slots: [], yolo: null });
  h.s.tabs = [tab]; h.s.pending_dialogs = []; h.store.applySnapshot(h.s);
  render(<Workspace client={h.client} connection={h.connection} launchContext="lc_default" squadAvailable={true} reconnect={vi.fn()} />);
  return { ...h, tab };
}
const answers = (send: ReturnType<typeof vi.fn>) => send.mock.calls.filter(([f]) => f.type === 'dialog_response');

describe('live countdown overlay', () => {
  it('opens from a tab Yolo delta, cancels with CancelCountdown, and closes when the countdown ends', () => {
    const h = mount();
    expect(screen.queryByRole('button', { name: 'Cancel countdown' })).toBeNull();
    act(() => h.store.applyDelta({ delta: 'yolo', value: { tab_id: h.tab.tab_id, state: yolo('implement', 12) } }));
    const overlay = screen.getByRole('dialog');
    expect(within(overlay).getByText('Step: implement')).toBeTruthy();
    fireEvent.click(within(overlay).getByRole('button', { name: 'Cancel countdown' }));
    expect(h.send).toHaveBeenCalledWith({ type: 'cancel_countdown', tab_id: h.tab.tab_id, step_name: 'implement' });
    fireEvent.click(within(overlay).getByRole('button', { name: 'Workflow' }));
    expect(h.send).toHaveBeenCalledWith({ type: 'workflow_request', tab_id: h.tab.tab_id, request: { request: 'open_control_board', step_name: 'implement' } });
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(h.send.mock.calls.filter(([f]) => f.type === 'cancel_countdown')).toHaveLength(2);
    fireEvent.keyDown(document, { key: 'y' }); fireEvent.keyDown(document, { key: 'Enter' });
    expect(answers(h.send)).toHaveLength(0);
    act(() => h.store.applyDelta({ delta: 'yolo', value: { tab_id: h.tab.tab_id, state: null } }));
    expect(screen.queryByRole('button', { name: 'Cancel countdown' })).toBeNull();
  });

  it('a normal engine dialog keeps precedence over the countdown', () => {
    const h = mount(t => ({ ...t, yolo: yolo('implement') }));
    act(() => h.emit({ type: 'dialog_request', dialog_id: 'dlg_q', tab_id: h.tab.tab_id, request: { ...request('yes_no'), title: 'Engine question' } }));
    expect(screen.getByRole('dialog', { name: 'Engine question' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Cancel countdown' })).toBeNull();
    act(() => h.emit({ type: 'dialog_closed', dialog_id: 'dlg_q', answered_by: { by: 'api' } }));
    expect(screen.getByRole('button', { name: 'Cancel countdown' })).toBeTruthy();
  });

  it('during a parallel group follows SlotYolo of the focused, maximized slot through Ctrl+S', () => {
    const base = snapshot().tabs[0]!.slots[0]!;
    const slot = (id: string, step: string, extra: Partial<SlotSnapshot> = {}): SlotSnapshot => ({ ...base, slot_id: id, step_name: step, terminal_id: null, acp: null, yolo: null, dormant: false, ...extra });
    const h = mount(t => ({ ...t, slots: [slot('slot_backbone', 'backbone', { dormant: true }), slot('slot_a', 'first'), slot('slot_b', 'second')], focused_slot: 'slot_a' }));
    expect(screen.queryByRole('button', { name: 'Cancel countdown' })).toBeNull();
    act(() => {
      h.store.applyDelta({ delta: 'slot_yolo', value: { tab_id: h.tab.tab_id, slot_id: 'slot_a', state: yolo('first') } });
      h.store.applyDelta({ delta: 'slot_yolo', value: { tab_id: h.tab.tab_id, slot_id: 'slot_b', state: yolo('second') } });
    });
    expect(within(screen.getByRole('dialog')).getByText('Step: first')).toBeTruthy();
    fireEvent.keyDown(document, { key: 's', ctrlKey: true });
    expect(within(screen.getByRole('dialog')).getByText('Step: second')).toBeTruthy();
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel countdown' }));
    expect(h.send).toHaveBeenLastCalledWith({ type: 'cancel_countdown', tab_id: h.tab.tab_id, step_name: 'second' });
    expect(answers(h.send)).toHaveLength(0);
  });

  it('fabricated per-slot countdown requests also follow the focused slot', () => {
    const base = snapshot().tabs[0]!.slots[0]!;
    const slot = (id: string, step: string): SlotSnapshot => ({ ...base, slot_id: id, step_name: step, terminal_id: null, acp: null, yolo: null, dormant: false });
    const h = mount(t => ({ ...t, slots: [slot('slot_a', 'first'), slot('slot_b', 'second')], focused_slot: 'slot_b' }));
    act(() => {
      h.emit({ type: 'dialog_request', dialog_id: 'dlg_first', tab_id: h.tab.tab_id, request: { ...request('workflow_yolo_countdown'), step_name: 'first' } });
      h.emit({ type: 'dialog_request', dialog_id: 'dlg_second', tab_id: h.tab.tab_id, request: { ...request('workflow_yolo_countdown'), step_name: 'second' } });
    });
    expect(within(screen.getByRole('dialog')).getByText('Step: second')).toBeTruthy();
  });
});

describe('liveCountdownState (TUI refresh_yolo_countdown_dialog parity)', () => {
  const base = snapshot().tabs[0]!;
  const s = (id: string, extra: Partial<SlotSnapshot>): SlotSnapshot => ({ ...base.slots[0]!, slot_id: id, yolo: null, dormant: false, ...extra });
  it('uses the tab countdown outside a parallel group', () => {
    expect(liveCountdownState({ ...base, yolo: yolo('x'), slots: [] }, undefined, () => 'maximized')?.state.step_name).toBe('x');
    expect(liveCountdownState({ ...base, yolo: null, slots: [] }, undefined, () => 'maximized')).toBeNull();
  });
  it('in a parallel group only shows the focused slot while maximized', () => {
    const focused = s('a', { yolo: yolo('a') });
    const tab = { ...base, yolo: yolo('tab'), slots: [s('backbone', { dormant: true }), focused] };
    expect(liveCountdownState(tab, focused, () => 'maximized')).toEqual({ state: yolo('a'), slotId: 'a' });
    expect(liveCountdownState(tab, focused, () => 'minimized')).toBeNull();
    expect(liveCountdownState(tab, s('b', {}), () => 'maximized')).toBeNull();
  });
});
