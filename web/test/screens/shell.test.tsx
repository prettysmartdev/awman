// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: () => <div>Contract terminal</div> }));
import { Workspace } from '@/components/Workspace';
import { StagePanes } from '@/lib/shell/StagePanes';
import { BINDINGS } from '@/lib/keys/keymap';
import { catalogueActions } from '@/lib/shell/actions';
import { harness, request } from './helpers';
afterEach(() => { cleanup(); sessionStorage.clear(); });
function show() {
  const h = harness(); h.s.tabs = [h.s.tabs[0]!]; h.s.tabs[0]!.workflow = null; h.store.applySnapshot(h.s);
  render(<Workspace client={h.client} connection={h.connection} launchContext="lc_default" squadAvailable reconnect={vi.fn()} />);
  return h;
}
it('resizes with the active touch and stops when that touch is canceled', () => {
  render(<StagePanes execution={<p>Output</p>} containers={<p>Terminal</p>} executionOpen containersOpen minimized={null} inert={false} />);
  const divider = screen.getByRole('separator');
  vi.spyOn(divider.parentElement!, 'getBoundingClientRect').mockReturnValue({ left: 0, width: 900 } as DOMRect);
  vi.spyOn(divider, 'getBoundingClientRect').mockReturnValue({ width: 12 } as DOMRect);
  fireEvent.touchStart(divider, { changedTouches: [{ identifier: 7, clientX: 300 }] });
  fireEvent.touchMove(document, { changedTouches: [{ identifier: 8, clientX: 600 }] });
  expect(divider.getAttribute('aria-valuenow')).toBe('33');
  fireEvent.touchMove(document, { changedTouches: [{ identifier: 7, clientX: 600 }] });
  expect(divider.getAttribute('aria-valuenow')).toBe('67');
  fireEvent.touchCancel(document, { changedTouches: [{ identifier: 7 }] });
  fireEvent.touchMove(document, { changedTouches: [{ identifier: 7, clientX: 100 }] });
  expect(divider.getAttribute('aria-valuenow')).toBe('67');
  expect(divider.parentElement!.getAttribute('data-resizing')).toBe('false');
});
it('catalogue selection fills the controlled composer and never submits', async () => {
  const h = show(); fireEvent.click(screen.getByRole('button', { name: 'Actions' }));
  const menu = screen.getByRole('dialog', { name: 'Actions' });
  fireEvent.change(within(menu).getByRole('searchbox'), { target: { value: 'ready' } });
  fireEvent.click(await within(menu).findByRole('button', { name: 'ready' }));
  await waitFor(() => expect((screen.getByRole('textbox', { name: 'Command' }) as HTMLTextAreaElement).value).toBe('ready '));
  expect(document.activeElement).toBe(screen.getByRole('textbox', { name: 'Command' }));
  expect(h.send.mock.calls.some(([f]) => f.type === 'submit')).toBe(false);
  fireEvent.click(screen.getByRole('button', { name: 'Run' }));
  expect(h.send).toHaveBeenCalledWith(expect.objectContaining({ type: 'submit', input: { source: 'command_box', raw: 'ready ' } }));
});
it('all shell shortcuts have visible command menu entries, including disabled actions', async () => {
  show(); fireEvent.click(screen.getByRole('button', { name: 'Actions' }));
  const menu = screen.getByRole('dialog', { name: 'Actions' });
  await within(menu).findByRole('button', { name: 'ready' });
  const present = new Set([...menu.querySelectorAll('[data-shortcut]')].map(e => e.getAttribute('data-shortcut')));
  const componentActions = /^(dialog\.|newTab\.|mountScope\.|keySetup\.|config\.newEntry|wfc\.|squad\.|nav\.|terminal\.|composer\.(submit|newline|complete))/;
  // Keyboard accelerators of controls that are visible where they apply
  // (scrolling the log, moving through workflow steps) stay out of Actions.
  const accelerators = /^(exec\.(scrollUp|scrollDown|pageUp|pageDown|top|bottom)|workflow\.overview(Left|Right))$/;
  for (const binding of BINDINGS) if (!componentActions.test(binding.action) && !accelerators.test(binding.action)) expect(present.has(binding.action), binding.action).toBe(true);
  for (const binding of BINDINGS) if (accelerators.test(binding.action)) expect(present.has(binding.action), binding.action).toBe(false);
});
it('shortcut-only accelerators stay in the shortcut list, and their matrix rows keep a present non-shortcut entry (L6)', async () => {
  const { readFileSync } = await import('node:fs'); const { join } = await import('node:path');
  const matrix = JSON.parse(readFileSync(join(__dirname, '../../acceptance/action-matrix.json'), 'utf8')) as { rows: { id: string; web_entry: { surface: string; state: string } }[] };
  const accelerators = BINDINGS.filter(b => /^(exec\.(scrollUp|scrollDown|pageUp|pageDown|top|bottom)|workflow\.overview(Left|Right))$/.test(b.action));
  expect(accelerators).toHaveLength(8);
  for (const binding of accelerators) for (const id of binding.matrix) {
    const row = matrix.rows.find(r => r.id === id);
    expect(row?.web_entry.state, id).toBe('present');
    expect(row?.web_entry.surface, id).not.toBe('command_menu');
  }
  show(); fireEvent.click(screen.getAllByRole('button', { name: 'Keyboard shortcuts' })[0]!);
  const sheet = screen.getByRole('dialog', { name: 'Keyboard shortcuts' });
  for (const binding of accelerators) expect(within(sheet).getAllByText(binding.label).length, binding.label).toBeGreaterThan(0);
});
it('running and disconnected workspaces explain disabled mutations while keeping navigation available', async () => {
  const h = show(); act(() => h.store.applyDelta({ delta: 'phase', value: { tab_id: h.s.tabs[0]!.tab_id, phase: { phase: 'running', command: 'ready' }, current_workflow: null, current_container: null } }));
  fireEvent.click(screen.getByRole('button', { name: 'Actions' }));
  const menu = screen.getByRole('dialog', { name: 'Actions' });
  expect((within(menu).getByRole('button', { name: 'Settings' }) as HTMLButtonElement).disabled).toBe(true);
  expect((await within(menu).findByRole('button', { name: 'ready' }) as HTMLButtonElement).disabled).toBe(true);
  act(() => h.state.set('reconnecting'));
  expect((within(menu).getByRole('button', { name: 'Stop command' }) as HTMLButtonElement).disabled).toBe(true);
  expect((within(menu).getByRole('button', { name: 'Git changes' }) as HTMLButtonElement).disabled).toBe(false);
  expect(within(menu).getAllByText('Reconnecting — actions are read only.').length).toBeGreaterThan(0);
});
it('mobile panel switching never answers, dismisses or reorders engine dialogs', () => {
  const h = show(); act(() => h.emit({ type: 'dialog_request', dialog_id: 'pending', tab_id: h.s.tabs[0]!.tab_id, request: request('yes_no') }));
  const dialog = screen.getByRole('dialog');
  const nav = screen.getByRole('navigation', { name: 'Panels' });
  for (const name of ['Git', 'Workflow', 'More', 'Agent', 'Output']) fireEvent.click(within(nav).getByRole('button', { name }));
  expect(screen.getByRole('dialog')).toBe(dialog);
  expect(h.store.get().dialogs[0]?.dialogId).toBe('pending');
  expect(h.send.mock.calls.some(([f]) => f.type === 'dialog_response')).toBe(false);
});
it('one inspector replaces another and Escape returns focus to its opener', async () => {
  show(); const git = screen.getAllByRole('button', { name: /^Git ·/ })[0]!; git.focus(); fireEvent.click(git);
  expect(screen.getAllByRole('complementary')).toHaveLength(1);
  const workflow = screen.getAllByRole('button', { name: 'Workflow' })[0]!; workflow.focus(); fireEvent.click(workflow);
  expect(screen.getAllByRole('complementary')).toHaveLength(1);
  expect(screen.getByRole('complementary', { name: 'workflow inspector' })).toBeTruthy();
  fireEvent.keyDown(document, { key: 'Escape' });
  await waitFor(() => expect(screen.queryByRole('complementary')).toBeNull());
  expect(document.activeElement).toBe(workflow);
});
it('catalogue groups are not runnable, but runnable parents and daemon stop remain discoverable', () => {
  const node = (path: string[], subcommands: ReturnType<typeof leaf>[] = []) => ({ path, subcommands, help: 'Help', long_help: null, aliases: [], flags: [], arguments: [] });
  function leaf(path: string[]) { return { path, subcommands: [], help: 'Help', long_help: null, aliases: [], flags: [], arguments: [] }; }
  const actions = catalogueActions({ etag: '', actions: {}, commands: [node(['daemon'], [leaf(['daemon', 'stop'])]), node(['squad'], [leaf(['squad', 'add'])])] }, vi.fn());
  expect(actions.map(a => a.label)).toEqual(['daemon stop', 'squad', 'squad add']);
  expect(actions[0]?.group).toBe('Danger');
});
it('queue checkout remains bound to its opening tab while the drawer permits tab navigation', async () => {
  const h = show(); const original = h.s.tabs[0]!;
  act(() => h.store.applyDelta({ delta: 'tab_opened', value: { ...original, tab_id: 'second', label: { text: 'Second tab', color: 'default' } } }));
  h.queueRequest.mockImplementation(async (_tab, req) => {
    if (req.operation === 'snapshot') return { result: 'snapshot', value: { items: [{ id: 'item', server: 'team', title: 'Queued task', summary: null, kind: 'task', labels: {}, priority: 1, age_ms: 0, for_you: false, body: null, source_url: null }], for_you: 0, new_since_seen: 0, error: null, loaded: true } };
    if (req.operation === 'pick') return { result: 'picked', value: { lease_id: 'lease', title: 'Queued task', server: 'team', cwd: '/repo', command_line: 'ready', argv: ['ready'] } };
    if (req.operation === 'checkout_offer') return { result: 'checkout_offer', value: true };
    return { result: 'done' };
  });
  fireEvent.click(screen.getByRole('button', { name: 'Queue' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Queued task · team' }));
  fireEvent.click(screen.getByRole('button', { name: 'Pick' }));
  await screen.findByText('Run in your checkout?');
  fireEvent.click(screen.getByRole('tab', { name: /Second tab/ }));
  fireEvent.click(screen.getByRole('button', { name: 'Run in a worktree of this checkout' }));
  await waitFor(() => expect(h.queueRequest).toHaveBeenCalledWith(original.tab_id, { operation: 'checkout', lease_id: 'lease', choice: 'worktree' }));
  expect(h.queueRequest).not.toHaveBeenCalledWith('second', expect.objectContaining({ operation: 'checkout' }));
});
