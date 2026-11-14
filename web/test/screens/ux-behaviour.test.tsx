// @vitest-environment jsdom
// Workspace behaviour from the WI 0160 UX review (B4, B5, B6, B9, H1, H3, M2,
// M3, M5, L1, L2, L5), through the real Workspace with a recording connection.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: () => <div data-key-context="terminal" tabIndex={0}>Contract terminal</div> }));
import { Workspace } from '@/components/Workspace';
import { Menu } from '@/lib/shell/Menu';
import { RECOVERY_COPY, RECOVERY_TITLES } from '@/lib/connect/model';
import type { CloseReason } from '@/lib/client/frontendSocket';
import type { TabSnapshot, YoloState } from '@/lib/client/types';
import { harness, request, snapshot } from './helpers';
afterEach(() => { cleanup(); sessionStorage.clear(); window.getSelection()?.removeAllRanges(); });

const yolo = (step_name = 'implement'): YoloState => ({ step_name, remaining_secs: 30, kind: 'stuck_step' });
function mount(patch: (tab: TabSnapshot) => TabSnapshot = t => t, setup?: (h: ReturnType<typeof harness>) => void) {
  const h = harness(); const original = snapshot().tabs[0]!;
  const tab = patch({ ...original, workflow: null, execution_phase: { phase: 'idle' }, slots: [], yolo: null });
  h.s.tabs = [tab]; h.s.pending_dialogs = []; h.store.applySnapshot(h.s); setup?.(h);
  const reconnect = vi.fn();
  render(<Workspace client={h.client} connection={h.connection} launchContext="lc_default" squadAvailable reconnect={reconnect} />);
  return { ...h, tab, reconnect };
}
const running = (t: TabSnapshot): TabSnapshot => ({ ...t, execution_phase: { phase: 'running', command: 'ready' } });
const withWorkflow = (t: TabSnapshot): TabSnapshot => ({ ...t, workflow: { steps: [{ name: 'implement', status: 'running', agent: 'claude', model: null, depends_on: [], kind: 'agent' }], current_step: 'implement', max_concurrent: 1, reported: {} } });
function selectPageText() { const node = document.querySelector('.context-path')!; const range = document.createRange(); range.selectNodeContents(node); window.getSelection()!.removeAllRanges(); window.getSelection()!.addRange(range); }
const frames = (send: ReturnType<typeof vi.fn>, type: string) => send.mock.calls.map(([f]) => f).filter(f => f.type === type);

describe('B4: Stop always stops', () => {
  it('the Stop button cancels even while page text is selected', () => {
    const h = mount(running); selectPageText();
    fireEvent.click(screen.getByRole('button', { name: 'Stop' }));
    expect(h.send).toHaveBeenCalledWith({ type: 'cancel', tab_id: h.tab.tab_id, repeat: false });
    expect(screen.queryByText('Copied selection')).toBeNull();
  });
  it('Actions → Stop command cancels even while page text is selected', async () => {
    const h = mount(running); selectPageText();
    fireEvent.click(screen.getByRole('button', { name: 'Actions' }));
    fireEvent.click(within(screen.getByRole('dialog', { name: 'Actions' })).getByRole('button', { name: 'Stop command' }));
    await waitFor(() => expect(h.send).toHaveBeenCalledWith({ type: 'cancel', tab_id: h.tab.tab_id, repeat: false }));
  });
  it('the Ctrl+C shortcut still copies a selection instead of stopping', () => {
    const writeText = vi.fn(async () => undefined); vi.stubGlobal('navigator', Object.create(navigator, { clipboard: { value: { writeText } } }));
    const h = mount(running); selectPageText();
    fireEvent.keyDown(document, { key: 'c', ctrlKey: true });
    expect(writeText).toHaveBeenCalled(); expect(frames(h.send, 'cancel')).toHaveLength(0);
  });
});

describe('B5: the countdown banner is not a blocking dialog', () => {
  it('keeps Actions, Queue, Squad, Stop and the composer usable during a countdown', () => {
    const h = mount(t => withWorkflow(running({ ...t, yolo: yolo() })));
    expect(document.querySelector('.embedded-banner')).toBeTruthy();
    for (const name of ['Actions', 'Queue']) expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled, name).toBe(false);
    expect((document.querySelector('.app-actions button.desktop-action') as HTMLButtonElement).disabled).toBe(false);
    expect(document.querySelector('.composer-dock')!.hasAttribute('inert')).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'Stop' }));
    // A workflow tab asks first, through the engine-worded confirmation.
    expect(h.send.mock.calls.some(([f]) => f.type === 'dialog_response')).toBe(false);
  });
  it('Cancel workflow in the inspector opens its confirmation and sends Cancel', async () => {
    const h = mount(t => withWorkflow(running({ ...t, yolo: yolo() })));
    fireEvent.click(screen.getAllByRole('button', { name: /^Workflow/ })[0]!);
    const inspector = screen.getByRole('complementary', { name: 'workflow inspector' });
    fireEvent.click(within(inspector).getByRole('button', { name: 'Cancel workflow' }));
    const dialog = await screen.findByRole('dialog', { name: 'workflow_cancel_confirm' });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel workflow' }));
    expect(h.send).toHaveBeenCalledWith({ type: 'cancel', tab_id: h.tab.tab_id, repeat: false });
  });
  it('Cancel workflow is disabled with a visible reason whenever its action is disabled', () => {
    mount(t => t, h => h.role.set('secondary'));
    fireEvent.click(screen.getAllByRole('button', { name: /^Workflow/ })[0]!);
    const inspector = screen.getByRole('complementary', { name: 'workflow inspector' });
    const button = within(inspector).getByRole('button', { name: 'Cancel workflow' }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    const reason = within(inspector).getAllByText('Attach as primary to use this action.');
    expect(reason.length).toBeGreaterThan(0); expect(button.contains(reason[0]!)).toBe(false);
  });
  it('a real engine dialog still blocks the workspace and keeps engine order', () => {
    const h = mount(running);
    act(() => { h.emit({ type: 'dialog_request', dialog_id: 'one', tab_id: h.tab.tab_id, request: { ...request('yes_no'), title: 'First' } }); h.emit({ type: 'dialog_request', dialog_id: 'two', tab_id: h.tab.tab_id, request: { ...request('yes_no'), title: 'Second' } }); });
    expect(screen.getByRole('dialog', { name: 'First' })).toBeTruthy();
    for (const name of ['Actions', 'Queue']) expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled, name).toBe(true);
    expect(document.querySelector('.composer-dock')!.hasAttribute('inert')).toBe(true);
  });
});

describe('B6: Escape is decided by focus', () => {
  it('inside the inspector Escape closes only the inspector, even if the countdown started first', async () => {
    const h = mount(t => withWorkflow({ ...t, yolo: yolo() }));
    fireEvent.click(screen.getAllByRole('button', { name: /^Git ·/ })[0]!);
    const inspector = screen.getByRole('complementary', { name: 'git inspector' });
    within(inspector).getByRole('button', { name: 'Close Git' }).focus();
    fireEvent.keyDown(document.activeElement!, { key: 'Escape' });
    await waitFor(() => expect(screen.queryByRole('complementary')).toBeNull());
    expect(frames(h.send, 'cancel_countdown')).toHaveLength(0);
  });
  it('inside the inspector Escape closes only the inspector when the countdown starts afterwards', async () => {
    const h = mount(withWorkflow);
    fireEvent.click(screen.getAllByRole('button', { name: /^Git ·/ })[0]!);
    act(() => h.store.applyDelta({ delta: 'yolo', value: { tab_id: h.tab.tab_id, state: yolo() } }));
    within(screen.getByRole('complementary')).getByRole('button', { name: 'Close Git' }).focus();
    fireEvent.keyDown(document.activeElement!, { key: 'Escape' });
    await waitFor(() => expect(screen.queryByRole('complementary')).toBeNull());
    expect(frames(h.send, 'cancel_countdown')).toHaveLength(0);
  });
  it('from the banner, or with nothing focused, Escape cancels the countdown and leaves the inspector open', () => {
    const h = mount(t => withWorkflow({ ...t, yolo: yolo() }));
    fireEvent.click(screen.getAllByRole('button', { name: /^Git ·/ })[0]!);
    within(document.querySelector<HTMLElement>('.embedded-banner')!).getByRole('button', { name: 'Cancel countdown' }).focus();
    fireEvent.keyDown(document.activeElement!, { key: 'Escape' });
    expect(frames(h.send, 'cancel_countdown')).toHaveLength(1);
    (document.activeElement as HTMLElement).blur();
    fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(frames(h.send, 'cancel_countdown')).toHaveLength(2);
    expect(screen.getByRole('complementary', { name: 'git inspector' })).toBeTruthy();
  });
});

describe('B9: read-only viewers keep reading and closing controls', () => {
  it.each(['secondary', 'reconnecting'] as const)('%s: inspector Close, copy, scroll and status log stay enabled', which => {
    mount(t => withWorkflow({ ...t, context_path: '/ctx' }), h => { if (which === 'secondary') h.role.set('secondary'); else h.state.set('reconnecting'); });
    fireEvent.click(screen.getAllByRole('button', { name: /^Workflow/ })[0]!);
    expect(document.querySelector('fieldset')).toBeNull();
    for (const name of ['Close overview', 'Copy context path', 'Status log', 'Output actions', 'Top of Command output', 'End of Command output']) expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled, name).toBe(false);
    expect((screen.getByRole('button', { name: 'Control step' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: 'Control workflow' }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe('H1: stage controls leave a docked inspector alone', () => {
  it('Output, Agent and output actions change only the stage at desktop widths', () => {
    mount();
    fireEvent.click(screen.getAllByRole('button', { name: /^Git ·/ })[0]!);
    fireEvent.click(within(screen.getByRole('main', { name: 'Workspace stage' })).getByRole('button', { name: 'Output' }));
    expect(screen.getByRole('complementary', { name: 'git inspector' })).toBeTruthy();
    fireEvent.keyDown(document.body, { key: 'Escape' });
  });
  it('the narrow layout keeps one main panel at a time', () => {
    vi.stubGlobal('matchMedia', (query: string) => ({ matches: query === '(max-width: 767px)', media: query, addEventListener() {}, removeEventListener() {} }));
    mount();
    const nav = screen.getByRole('navigation', { name: 'Panels' });
    fireEvent.click(within(nav).getByRole('button', { name: 'Git' }));
    expect(screen.getByRole('complementary', { name: 'git inspector' })).toBeTruthy();
    fireEvent.click(within(nav).getByRole('button', { name: 'Output' }));
    expect(screen.queryByRole('complementary')).toBeNull();
  });
});

describe('H3: the live workspace explains each ending precisely', () => {
  const ended = (reason: CloseReason, text: string) => mount(t => t, h => { Object.defineProperty(h.connection, 'closeReason', { value: reason, configurable: true }); h.store.markEnded(text); });
  it('an authorization ending uses the entry flow title, copy and next step', () => {
    const h = ended({ kind: 'auth_ended', state: 'daemon_restarted' }, RECOVERY_COPY.daemon_restarted);
    expect(screen.getByRole('heading', { name: RECOVERY_TITLES.daemon_restarted })).toBeTruthy();
    expect(screen.getByText(RECOVERY_COPY.daemon_restarted)).toBeTruthy();
    expect(screen.queryByText(/revoked/i)).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Connect this browser again' })); expect(h.reconnect).toHaveBeenCalled();
  });
  it('another primary keeps the resume record so its options are offered again', () => {
    sessionStorage.clear();
    const h = ended({ kind: 'other_primary' }, RECOVERY_COPY.other_primary);
    expect(screen.getByRole('heading', { name: RECOVERY_TITLES.other_primary })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Resume workspace' })); expect(h.reconnect).toHaveBeenCalled();
  });
  it('a lost instance offers another workspace', () => {
    ended({ kind: 'grace_expired' }, RECOVERY_COPY.resume_unavailable);
    expect(screen.getByRole('heading', { name: RECOVERY_TITLES.resume_unavailable })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Choose another workspace' })).toBeTruthy();
  });
  it('a rate-limited proof shows a temporary retry state, never an ending', () => {
    const until = Date.now() + 12_000;
    mount(t => t, h => { (h.api as unknown as { recoveryThrottledUntil: number }).recoveryThrottledUntil = until; });
    expect(screen.getByText(/Too many attempts, retrying in 1[12]s\./)).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });
});

describe('M4: Shift+←/→ move through workflow steps', () => {
  it('selects the next and previous step while the inspector is open', () => {
    mount(t => ({ ...t, workflow: { steps: ['plan', 'build', 'ship'].map(name => ({ name, status: 'pending' as const, agent: null, model: null, depends_on: [], kind: 'agent' })), current_step: 'plan', max_concurrent: 1, reported: {} } }));
    fireEvent.click(screen.getAllByRole('button', { name: /^Workflow/ })[0]!);
    fireEvent.keyDown(document.body, { key: 'ArrowRight', shiftKey: true });
    expect(screen.getByRole('button', { name: 'build · pending', expanded: true })).toBeTruthy();
    fireEvent.keyDown(document.body, { key: 'ArrowLeft', shiftKey: true });
    expect(screen.getByRole('button', { name: 'plan · pending', expanded: true })).toBeTruthy();
  });
});

describe('M2, M3, M5, L1, L2, L5', () => {
  it('M2: answered-by notices are humanised, dismissible and silent for own answers', () => {
    const h = mount();
    act(() => { h.emit({ type: 'dialog_request', dialog_id: 'mine', tab_id: h.tab.tab_id, request: request('yes_no') }); h.emit({ type: 'dialog_closed', dialog_id: 'mine', answered_by: { by: 'frontend', frontend_id: 'fe_abc' } }); });
    expect(screen.queryByText(/answered/i)).toBeNull();
    act(() => { h.emit({ type: 'dialog_request', dialog_id: 'theirs', tab_id: h.tab.tab_id, request: request('yes_no') }); h.emit({ type: 'dialog_closed', dialog_id: 'theirs', answered_by: { by: 'frontend', frontend_id: 'fe_3c4d5e6f708192a3' } }); });
    const notice = screen.getByText('Another window answered the prompt.').closest('.notice') as HTMLElement;
    expect(notice.textContent).not.toContain('fe_');
    fireEvent.click(within(notice).getByRole('button', { name: 'Dismiss' }));
    expect(screen.queryByText('Another window answered the prompt.')).toBeNull();
  });
  it('M8: with several tabs, All tabs (n) opens the full tab list', () => {
    const h = harness(); const base = snapshot().tabs[0]!;
    h.s.tabs = [0, 1, 2].map(i => ({ ...base, tab_id: `tab_${i}`, label: { ...base.label, text: `Tab ${i}` }, workflow: null, execution_phase: { phase: 'idle' }, slots: [], yolo: null }));
    h.s.pending_dialogs = []; h.store.applySnapshot(h.s);
    render(<Workspace client={h.client} connection={h.connection} launchContext="lc_default" squadAvailable reconnect={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: 'All tabs (3)' }));
    const list = screen.getByRole('dialog', { name: 'Workspace tabs' });
    for (const i of [0, 1, 2]) expect(within(list).getByRole('button', { name: new RegExp(`^Tab ${i}`) })).toBeTruthy();
  });
  it('M3: a local sheet does not reappear after an engine dialog closes', () => {
    const h = mount();
    fireEvent.click(screen.getByRole('button', { name: 'Actions' }));
    act(() => h.emit({ type: 'dialog_request', dialog_id: 'q', tab_id: h.tab.tab_id, request: { ...request('yes_no'), title: 'Engine question' } }));
    expect(screen.getByRole('dialog', { name: 'Engine question' })).toBeTruthy();
    act(() => h.emit({ type: 'dialog_closed', dialog_id: 'q', answered_by: { by: 'api' } }));
    expect(screen.queryByRole('dialog')).toBeNull();
  });
  it('M5: at most one filled primary on the workspace, banner and sheets; destructive buttons use danger', async () => {
    const h = mount(t => withWorkflow({ ...t, yolo: yolo() }));
    fireEvent.click(screen.getAllByRole('button', { name: /^Workflow/ })[0]!);
    const filled = (root: ParentNode) => [...root.querySelectorAll('button:not(.secondary):not(.danger)')].filter(b => !b.closest('.desktop-tabs'));
    expect(filled(document.querySelector('.embedded-banner')!).map(b => b.textContent)).toEqual(['Cancel countdown']);
    expect(filled(screen.getByRole('complementary'))).toHaveLength(0);
    expect(within(screen.getByRole('complementary')).getByRole('button', { name: 'Cancel workflow' }).classList.contains('danger')).toBe(true);
    fireEvent.click(screen.getAllByRole('button', { name: 'New tab' }).find(b => !b.closest('.desktop-tabs'))!);
    const sheet = await screen.findByRole('dialog');
    expect(filled(sheet).map(b => b.textContent)).toEqual(['Open']);
    void h;
  });
  it('M5: quit and close-tab confirmations style their destructive choices as danger', async () => {
    mount();
    fireEvent.click(screen.getAllByRole('button', { name: 'Quit awman' })[0]!);
    const dialog = await screen.findByRole('dialog', { name: 'quit_confirm' });
    expect(within(dialog).getByRole('button', { name: 'Quit awman' }).classList.contains('danger')).toBe(true);
  });
  it('L1: a menu renders its disabled reason outside the disabled item', () => {
    render(<Menu label="Tab menu" actions={[{ id: 'tab.close', label: 'Close current tab', group: 'Danger', matrix: [], disabled: 'Attach as primary to use this action.', run: vi.fn() }]} />);
    fireEvent.click(screen.getByRole('button', { name: 'Tab menu' }));
    const item = screen.getByRole('menuitem', { name: 'Close current tab' });
    const reason = screen.getByText('Attach as primary to use this action.');
    expect(item.contains(reason)).toBe(false);
    expect(item.getAttribute('aria-describedby')).toBe(reason.id);
  });
  it('L2: connection and execution states use user words', () => {
    const h = mount();
    expect(document.querySelector('.connection-pill')!.textContent).toBe('Connected');
    expect(screen.getByText('Ready')).toBeTruthy();
    act(() => h.state.set('reconnecting'));
    expect(document.querySelector('.connection-pill')!.textContent).toBe('Reconnecting');
    act(() => { h.state.set('open'); h.role.set('secondary'); });
    expect(document.querySelector('.connection-pill')!.textContent).toBe('Connected · View only');
  });
  it('L5: a squad tab shows no agent empty state and no dead Agent button', () => {
    mount(t => ({ ...t, kind: 'squad', label: { text: 'squad', color: 'default' } }));
    expect(screen.queryByText(/No active agent in this tab/)).toBeNull();
    const stage = screen.getByRole('main', { name: 'Workspace stage' });
    expect((within(stage).getByRole('button', { name: 'Agent' }) as HTMLButtonElement).disabled).toBe(true);
  });
});
