// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: ({ title, windowActions = [] }: { title: string; windowActions?: { id: string; label: string; run(): void }[] }) => <div>Hub terminal: {title}{windowActions.map(a => <button key={a.id} onClick={a.run}>{a.label}</button>)}</div> }));
import { ContainerArea, ExecutionLog, GitPanel, NOTICE_EXPIRY_MS, NoticeExpiry, TabBar, WorkflowPanel, statusGlyph } from '@/components/Panels';
import { VirtualList } from '@/components/ui';
import { snapshot, harness } from './helpers';
afterEach(cleanup);
it('renders labels, colours, statuses, yolo and responsive overflow tabs', () => { const s = snapshot(); const tab = s.tabs[0]!; tab.stuck = true; tab.yolo = { step_name: 'one', remaining_secs: 9, kind: 'stuck_step' }; const select = vi.fn(); render(<TabBar tabs={s.tabs} active={tab.tab_id} select={select} close={vi.fn()} newTab={vi.fn()} readOnly={false} />); fireEvent.click(screen.getAllByRole('tab')[0]!); expect(select).toHaveBeenCalledWith(tab.tab_id); expect(screen.getAllByRole('tab')[0]!.textContent).toContain('9s'); expect(screen.getAllByRole('tab')[0]!.textContent).toContain('⚠'); expect(statusGlyph({ state: 'blocked', kind: 'permission', progress: null, msg: 'allow', app: null })).toContain('🔒'); });
it('virtualizes 50k log rows and renders inert output, errors and last summary', () => { const tab = snapshot().tabs[0]!; const entries = Array.from({ length: 50_000 }, (_, i) => ({ kind: 'text' as const, stream: 'stdout' as const, level: 'info' as const, text: `<img>line ${i}` })); render(<ExecutionLog tab={tab} entries={entries} showStatus={true} toggleStatus={vi.fn()} stopWatch={vi.fn()} copy={vi.fn()} />); const list = screen.getByLabelText('Command output'); expect(list.querySelectorAll('.virtual-row').length).toBeLessThanOrEqual(40); expect(document.querySelector('img')).toBeNull(); Object.defineProperty(list, 'scrollTop', { value: 49_970 * 28, writable: true }); fireEvent.scroll(list); expect(screen.getByText('<img>line 49999')).toBeTruthy(); });
it('git reflects snapshot summaries and paths are only text', () => { const tab = snapshot().tabs[0]!; tab.git_summary = { branch: '<b>feature</b>', added: 9, removed: 1, files: [{ path: '<script>x</script>', change: 'added', added: 9, removed: 1, binary: false }] }; render(<GitPanel tab={tab} close={vi.fn()} />); expect(screen.getByRole('heading').textContent).toContain('<b>feature</b>'); expect(document.querySelector('script')).toBeNull(); expect(screen.getAllByText(/9.*−1/).length).toBe(2); });
it('workflow step boxes preserve dependencies, agent reports and board target', () => { const tab = snapshot().tabs[0]!; const control = vi.fn(); tab.workflow = { current_step: 'build', max_concurrent: 2, reported: { build: 'checking' }, steps: [{ name: 'build', status: 'running', agent: 'codex', model: 'model', depends_on: ['plan'], kind: 'agent' }] }; render(<WorkflowPanel tab={tab} close={vi.fn()} control={control} />); expect(screen.getByText('Depends on: plan')).toBeTruthy(); expect(screen.getByText('checking')).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Control step' })); expect(control).toHaveBeenCalledWith('build'); });
it('parallel agents stay mounted while only the selected one is visible, with window actions per agent', () => { const h = harness(); const tab = snapshot().tabs[0]!; const slot = tab.slots[0]!; tab.slots = [slot, { ...slot, slot_id: 'slot_second', step_name: 'second' }]; const focus = vi.fn(); const window = vi.fn(); const view = render(<ContainerArea tab={tab} client={h.client} readOnly={false} focused={slot.slot_id} setFocused={focus} windows={{}} setWindow={window} />); const terminals = screen.getAllByText(/Hub terminal:/); expect(terminals.length).toBe(2); expect(terminals[0]!.closest('[hidden]')).toBeNull(); expect(terminals[1]!.closest('[hidden]')).not.toBeNull(); fireEvent.click(screen.getAllByRole('button', { name: 'Collapse agent', hidden: true })[1]!); expect(window).toHaveBeenCalledWith('slot_second', 'minimized'); fireEvent.click(screen.getAllByRole('button', { name: 'Hide agent', hidden: true })[0]!); expect(window).toHaveBeenCalledWith(slot.slot_id, 'hidden'); view.rerender(<ContainerArea tab={tab} client={h.client} readOnly={false} focused='slot_second' setFocused={focus} windows={{}} setWindow={window} />); const after = screen.getAllByText(/Hub terminal:/); expect(after[0]).toBe(terminals[0]); expect(after[0]!.closest('[hidden]')).not.toBeNull(); expect(after[1]!.closest('[hidden]')).toBeNull(); });
it('large lists keep a bounded DOM with native scroll positions', () => { render(<VirtualList label="Big list" items={Array.from({ length: 10000 }, (_, i) => i)} render={n => <span>Entry {n}</span>} />); expect(document.querySelectorAll('.virtual-row').length).toBe(40); });
it('renders the agents dashboard table, crash status lines and final container summary', () => { const tab = snapshot().tabs[0]!; tab.agents_dashboard = { watching: true, overview: { runtime_display: 'Docker', available: true, agents: [{ name: 'agent-one', state: 'working' }] } }; tab.status_log = [{ level: 'error', text: 'Agent crashed; see the log' }]; tab.last_container_summary = { agent_display_name: 'Agent One', container_name: 'container-one', avg_cpu: '5%', avg_memory: '64MB', total_time: '3m', exit_code: 2 }; const stop = vi.fn(); render(<ExecutionLog tab={tab} entries={[]} showStatus={true} toggleStatus={vi.fn()} stopWatch={stop} copy={vi.fn()} />); expect(screen.getByRole('table').textContent).toContain('Docker'); expect(screen.getByText(/name: agent-one/)).toBeTruthy(); expect(screen.getByText('Agent crashed; see the log')).toBeTruthy(); expect(screen.getByText(/Agent One.*exit 2/)).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Output actions' })); fireEvent.click(screen.getByRole('menuitem', { name: 'Stop watching' })); expect(stop).toHaveBeenCalled(); });
it('keeps a long vertical workflow bounded while retaining all step details', () => { const tab = snapshot().tabs[0]!; tab.workflow = { current_step: 'step0', reported: {}, max_concurrent: 4, steps: Array.from({ length: 1000 }, (_, i) => ({ name: `step${i}`, status: 'pending', kind: 'agent', agent: 'codex', model: null, depends_on: [] })) }; render(<WorkflowPanel tab={tab} close={vi.fn()} control={vi.fn()} />); expect(document.querySelectorAll('.workflow-step-row').length).toBe(40); const strip = screen.getByLabelText('Workflow steps'); Object.defineProperty(strip, 'scrollTop', { value: 44_000, writable: true }); fireEvent.scroll(strip); fireEvent.click(screen.getByRole('button', { name: 'step999 · pending · codex' })); expect(screen.getByRole('region', { name: 'Step step999' })).toBeTruthy(); });
it('workflow steps are compact rows in one scroll region; the selected step expands (M4)', () => {
  const tab = snapshot().tabs[0]!; const control = vi.fn();
  tab.workflow = { current_step: 'step3', reported: { step5: 'reviewing' }, max_concurrent: 1, steps: Array.from({ length: 14 }, (_, i) => ({ name: `step${i}`, status: i < 3 ? 'done' : i === 3 ? 'running' : 'pending', kind: 'agent', agent: 'claude', model: 'opus', depends_on: i ? [`step${i - 1}`] : [] })) };
  render(<WorkflowPanel tab={tab} close={vi.fn()} control={control} />);
  const list = screen.getByRole('group', { name: 'Workflow steps' });
  // Every step is a one-line row and the list is not a nested scroll region.
  expect(within(list).getAllByRole('button', { expanded: false }).length + within(list).getAllByRole('button', { expanded: true }).length).toBe(14);
  expect(list.closest('.virtual-list')).toBeNull(); expect(document.querySelector('.virtual-list')).toBeNull();
  expect(screen.getByRole('button', { name: 'step3 · running · claude', expanded: true })).toBeTruthy();
  expect(screen.getAllByRole('region')).toHaveLength(1);
  fireEvent.click(screen.getByRole('button', { name: 'step5 · pending · claude' }));
  const detail = screen.getByRole('region', { name: 'Step step5' });
  expect(within(detail).getByText('Depends on: step4')).toBeTruthy(); expect(within(detail).getByText('reviewing')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'step3 · running · claude', expanded: false })).toBeTruthy();
  fireEvent.click(within(detail).getByRole('button', { name: 'Control step' })); expect(control).toHaveBeenCalledWith('step5');
  fireEvent.click(screen.getByRole('button', { name: 'step5 · pending · claude' })); expect(screen.queryByRole('region')).toBeNull();
});
it('a read-only workflow inspector disables only mutating controls (B9)', () => {
  const tab = snapshot().tabs[0]!; tab.context_path = '/ctx/path';
  tab.workflow = { current_step: 'build', reported: {}, max_concurrent: 1, steps: [{ name: 'build', status: 'running', agent: 'codex', model: null, depends_on: [], kind: 'agent' }] };
  render(<WorkflowPanel tab={tab} close={vi.fn()} control={vi.fn()} readOnly />);
  expect((screen.getByRole('button', { name: 'Control step' }) as HTMLButtonElement).disabled).toBe(true);
  for (const name of ['Close overview', 'Copy context path', 'build · running · codex']) expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled, name).toBe(false);
});
it('a read-only execution log keeps reading and copying controls (B9, L2)', () => {
  const tab = snapshot().tabs[0]!; tab.execution_phase = { phase: 'idle' }; tab.agents_dashboard = { watching: true, overview: {} };
  render(<ExecutionLog tab={tab} entries={[]} showStatus={false} toggleStatus={vi.fn()} stopWatch={vi.fn()} copy={vi.fn()} readOnly />);
  expect(screen.getByText('Ready')).toBeTruthy();
  for (const name of ['Status log', 'Output actions', 'Top of Command output', 'End of Command output']) expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled, name).toBe(false);
  fireEvent.click(screen.getByRole('button', { name: 'Output actions' }));
  expect((screen.getByRole('menuitem', { name: 'Copy selection' }) as HTMLButtonElement).disabled).toBe(false);
  expect((screen.getByRole('menuitem', { name: 'Stop watching' }) as HTMLButtonElement).disabled).toBe(true);
});
it('renders ACP text and plan messages without interpreting output or losing long content', () => {
  const h = harness(); const tab = snapshot().tabs[0]!; const slot = tab.slots[0]!;
  slot.terminal_id = null; slot.acp = { pending_permission: null, history: [{ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: '<script>untrusted</script>' } }, { sessionUpdate: 'plan', entries: [{ content: 'Read the long report', priority: 'high', status: 'in_progress' }] }] };
  render(<ContainerArea tab={tab} client={h.client} readOnly focused={slot.slot_id} setFocused={vi.fn()} windows={{}} setWindow={vi.fn()} />);
  expect(screen.getByText('<script>untrusted</script>')).toBeTruthy(); expect(document.querySelector('script')).toBeNull();
  expect(screen.getByText('in_progress · Read the long report')).toBeTruthy();
});
it('expires info and success notices after a delay but keeps warnings and errors (M2)', () => {
  vi.useFakeTimers(); const dismiss = vi.fn();
  const view = render(<NoticeExpiry notices={[{ id: 1, level: 'info' }, { id: 2, level: 'error' }, { id: 3, level: 'warning' }]} dismiss={dismiss} />);
  vi.advanceTimersByTime(NOTICE_EXPIRY_MS - 1); expect(dismiss).not.toHaveBeenCalled();
  view.rerender(<NoticeExpiry notices={[{ id: 1, level: 'info' }, { id: 2, level: 'error' }, { id: 3, level: 'warning' }, { id: 4, level: 'success' }]} dismiss={dismiss} />);
  vi.advanceTimersByTime(1); expect(dismiss.mock.calls).toEqual([[1]]); // a new notice does not restart older timers
  vi.advanceTimersByTime(NOTICE_EXPIRY_MS); expect(dismiss.mock.calls).toEqual([[1], [4]]);
  vi.useRealTimers();
});
