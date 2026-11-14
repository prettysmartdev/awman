// @vitest-environment jsdom
// Row-keyed behaviour tests for matrix rows the web review marked PARTIAL for
// a missing row-specific assertion. Each title starts with the matrix row id.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: () => <div data-key-context="terminal" tabIndex={0}>Contract terminal</div> }));
import { DialogRenderer } from '@/components/Dialogs';
import { ExecutionLog } from '@/components/Panels';
import { Squad } from '@/components/Squad';
import { Workspace } from '@/components/Workspace';
import type { DialogRequest } from '@/lib/client/types';
import { harness, request, snapshot } from './helpers';
afterEach(() => { cleanup(); sessionStorage.clear(); });

function dialog(r: DialogRequest) {
  const h = harness();
  render(<DialogRenderer dialog={{ dialogId: 'dlg_row', tabId: h.s.tabs[0]!.tab_id, request: r }} client={h.client} connection={h.connection} />);
  return h;
}
const lastFrame = (send: ReturnType<typeof vi.fn>) => send.mock.calls.at(-1)?.[0];

describe('matrix rows', () => {
  it('repo.new_tab_cancel: the Cancel button closes the dialog and sends nothing', async () => {
    const h = harness(); h.s.tabs = [{ ...h.s.tabs[0]!, workflow: null }]; h.store.applySnapshot(h.s);
    render(<Workspace client={h.client} connection={h.connection} launchContext="lc_default" squadAvailable={true} reconnect={vi.fn()} />);
    fireEvent.click(screen.getAllByRole('button', { name: 'New tab' })[0]!);
    await screen.findByRole('option', { name: /Project/ });
    const before = h.send.mock.calls.length;
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: /Cancel|Close|Dismiss/ }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(h.send.mock.calls.slice(before).filter(([f]) => f.type === 'open_tab')).toEqual([]);
  });

  it('exec.status_log_toggle: the Status log control reveals and hides the status messages', () => {
    const h = harness(); h.s.tabs = [{ ...h.s.tabs[0]!, workflow: null, status_log: [{ level: 'warning', text: 'container restarted' }] }]; h.store.applySnapshot(h.s);
    render(<Workspace client={h.client} connection={h.connection} launchContext="lc_default" squadAvailable={true} reconnect={vi.fn()} />);
    const toggle = screen.getByRole('button', { name: 'Status log' });
    expect(screen.queryByText('container restarted')).toBeNull();
    fireEvent.click(toggle);
    expect(toggle.getAttribute('aria-pressed')).toBe('true');
    expect(screen.getByText('container restarted')).toBeTruthy();
    fireEvent.click(toggle);
    expect(screen.queryByText('container restarted')).toBeNull();
  });

  it('exec.output: CommandBytes split mid-character render as one assembled line', () => {
    const h = harness(); const tab = h.s.tabs[0]!;
    h.store.appendBytes(tab.tab_id, 'stdout', new Uint8Array([0x70, 0x72, 0xe2, 0x82]));
    h.store.appendBytes(tab.tab_id, 'stdout', new Uint8Array([0xac, 0x0a]));
    render(<ExecutionLog tab={tab} entries={h.store.get().outputs.get(tab.tab_id) ?? []} showStatus={false} toggleStatus={vi.fn()} stopWatch={vi.fn()} copy={vi.fn()} />);
    expect(screen.getByText('pr€')).toBeTruthy();
    expect(screen.queryByText(/�/)).toBeNull();
  });

  it('appr.step_confirm_advance: Next and Enter send Char(>)', () => {
    const h = dialog(request('workflow_step_confirm'));
    fireEvent.click(screen.getByRole('button', { name: 'Next' }));
    expect(lastFrame(h.send)).toEqual({ type: 'dialog_response', dialog_id: 'dlg_row', response: { kind: 'char', value: '>' } });
    cleanup();
    const k = dialog(request('workflow_step_confirm'));
    fireEvent.keyDown(document, { key: 'Enter' });
    expect(lastFrame(k.send)).toEqual({ type: 'dialog_response', dialog_id: 'dlg_row', response: { kind: 'char', value: '>' } });
  });

  it('appr.step_confirm_pause: Pause and Esc send Dismissed', () => {
    const h = dialog(request('workflow_step_confirm'));
    fireEvent.click(screen.getByRole('button', { name: 'Pause' }));
    expect(lastFrame(h.send)).toEqual({ type: 'dialog_response', dialog_id: 'dlg_row', response: { kind: 'dismissed' } });
    cleanup();
    const k = dialog(request('workflow_step_confirm'));
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(lastFrame(k.send)).toEqual({ type: 'dialog_response', dialog_id: 'dlg_row', response: { kind: 'dismissed' } });
  });

  it('wfc.dismiss: Esc on a dismissable board sends Dismissed', () => {
    const h = dialog(request('workflow_control_board'));
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(lastFrame(h.send)).toEqual({ type: 'dialog_response', dialog_id: 'dlg_row', response: { kind: 'dismissed' } });
  });

  it('wfc.parallel_labels: the board shows the focused step and running peer counts of a parallel group', () => {
    dialog({ ...request('workflow_control_board'), focused_step_name: 'review-b', parallel_peer_count: 3, parallel_peers_running: 2, in_parallel_group: true });
    expect(screen.getByText(/Focused step: review-b · 2\/3 peers running · parallel group/)).toBeTruthy();
  });

  it('squad.resume: Resume on a paused task PATCHes paused:false without a confirm', async () => {
    const h = harness(); h.api.squad.tasks.mockResolvedValue([{ ...h.task, status: 'paused' }]);
    render(<Squad client={h.client} available={true} readOnly={false} submit={vi.fn()} detach={vi.fn()} />);
    await screen.findByRole('button', { name: /Daily.*paused/ });
    fireEvent.click(screen.getByRole('button', { name: 'Task actions' })); fireEvent.click(screen.getByRole('menuitem', { name: 'Resume' }));
    await waitFor(() => expect(h.api.squad.patch).toHaveBeenCalledWith(h.task.id, { paused: false }));
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('tabs.stuck_yellow: a stuck delta marks the tab and its slot', () => {
    const h = harness(); const tab = { ...snapshot().tabs[0]!, workflow: null }; h.s.tabs = [tab]; h.store.applySnapshot(h.s);
    render(<Workspace client={h.client} connection={h.connection} launchContext="lc_default" squadAvailable={true} reconnect={vi.fn()} />);
    expect(screen.getByRole('tab').textContent).not.toContain('⚠');
    act(() => h.store.applyDelta({ delta: 'stuck', value: { tab_id: tab.tab_id, stuck: true } }));
    expect(screen.getByRole('tab').textContent).toContain('⚠');
  });
});
