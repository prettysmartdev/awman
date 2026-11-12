// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: () => <div>Task terminal</div> }));
import { Squad } from '@/components/Squad';
import { harness } from './helpers';
afterEach(cleanup);
function taskAction(name: string) { fireEvent.click(screen.getByRole('button', { name: 'Task actions' })); fireEvent.click(screen.getByRole('menuitem', { name })); }
async function show() { const h = harness(); const submit = vi.fn(); render(<Squad client={h.client} available={true} readOnly={false} submit={submit} detach={vi.fn()} />); await screen.findByRole('button', { name: /Daily.*active/ }); return { ...h, submit }; }
it('shows unavailable recipes for missing listener features', () => { const h = harness(); render(<Squad client={h.client} available={false} readOnly={false} submit={vi.fn()} detach={vi.fn()} />); expect(screen.getByText('awman config set daemon.network.listeners.loopback.features engine,squad')).toBeTruthy(); expect(screen.getByText('awman config set daemon.features.squad.enabled true')).toBeTruthy(); expect(h.api.squad.tasks).not.toHaveBeenCalled(); });
it('renders cards, details, history and inactive command reminder', async () => { const h = await show(); expect(screen.getByText(/command box is inactive/)).toBeTruthy(); taskAction('Details'); await screen.findByRole('heading', { name: 'Task details' }); expect(h.api.squad.task).toHaveBeenCalledWith(h.task.id); fireEvent.click(screen.getByRole('button', { name: 'Close details' })); taskAction('History'); await screen.findByRole('dialog', { name: 'Run history' }); expect(h.api.squad.runs).toHaveBeenCalledWith(h.task.id); });
it.each([['Pause', 'patch'], ['Trigger', 'trigger'], ['Cancel run', 'cancelRun']] as const)('requires engine prompt before %s', async (label, method) => { const h = await show(); taskAction(label); await screen.findByText('Engine wording for squad_task_confirm'); expect(h.api.squad[method]).not.toHaveBeenCalled(); fireEvent.click(screen.getByRole('button', { name: 'Confirm task action' })); await waitFor(() => expect(h.api.squad[method]).toHaveBeenCalledTimes(1)); });
it('new and edit actions use the engine interview; attach uses argv with no shell interpolation', async () => { const h = await show(); fireEvent.click(screen.getByRole('button', { name: 'New task' })); await waitFor(() => expect(h.submit).toHaveBeenCalledWith(['squad', 'add', '--interview'])); taskAction('Edit'); await waitFor(() => expect(h.submit).toHaveBeenCalledWith(['squad', 'edit', h.task.name, '--interview'])); taskAction('Attach'); await waitFor(() => expect(h.api.terminals).toHaveBeenCalledWith(h.task.id)); expect(h.submit).toHaveBeenCalledWith(['squad', 'attach', h.task.name]); });
it('delete confirmation can be declined and confirmed', async () => { const h = await show(); taskAction('Delete task'); const dialog = await screen.findByRole('dialog', { name: 'Delete task' }); fireEvent.click(dialog.querySelectorAll('button')[1]!); expect(h.api.squad.delete).not.toHaveBeenCalled(); taskAction('Delete task'); const next = await screen.findByRole('dialog', { name: 'Delete task' }); fireEvent.click(next.querySelectorAll('button')[0]!); await waitFor(() => expect(h.api.squad.delete).toHaveBeenCalledWith(h.task.id)); });
it('displays actionable route errors without optimistic task changes', async () => { const h = await show(); h.api.squad.patch.mockRejectedValueOnce(new Error('conflict: task changed')); taskAction('Pause'); fireEvent.click(await screen.findByRole('button', { name: 'Confirm task action' })); await screen.findByText('conflict: task changed'); expect(screen.getByRole('button', { name: /Daily.*active/ })).toBeTruthy(); });
it('detach asks before canceling the attach and leaves the task running', async () => { const h = harness(); const detach = vi.fn(); render(<Squad client={h.client} available={true} readOnly={false} submit={vi.fn()} detach={detach} />); await screen.findByRole('button', { name: /Daily.*active/ }); taskAction('Attach'); await screen.findByText(/Waiting for the task terminal/); fireEvent.keyDown(document, { key: 'c', ctrlKey: true }); await screen.findByRole('dialog', { name: 'Detach from task?' }); expect(detach).not.toHaveBeenCalled(); fireEvent.click(screen.getByRole('dialog').querySelector('button')!); expect(detach).toHaveBeenCalledTimes(1); expect(h.api.squad.cancelRun).not.toHaveBeenCalled(); });
it('visible Detach button clears the attachment so the same task can be re-attached (review #5)', async () => {
  const h = harness(); const detach = vi.fn(); const submit = vi.fn(); const onAttachmentChange = vi.fn();
  (h.api.terminals as unknown as ReturnType<typeof vi.fn>).mockResolvedValue([{ id: 'term_task', state: 'open' }]);
  render(<Squad client={h.client} available={true} readOnly={false} submit={submit} detach={detach} onAttachmentChange={onAttachmentChange} />);
  await screen.findByRole('button', { name: /Daily.*active/ });
  taskAction('Attach');
  await screen.findByText('Task terminal');
  expect(onAttachmentChange).toHaveBeenLastCalledWith(true);
  fireEvent.click(screen.getByRole('button', { name: 'Detach' }));
  fireEvent.click(within(await screen.findByRole('dialog', { name: 'Detach from task?' })).getByRole('button', { name: 'Detach' }));
  expect(detach).toHaveBeenCalledTimes(1);
  expect(screen.queryByText('Task terminal')).toBeNull();
  expect(screen.queryByText(/Waiting for the task terminal/)).toBeNull();
  expect(onAttachmentChange).toHaveBeenLastCalledWith(false);
  const polls = h.api.terminals.mock.calls.length;
  taskAction('Attach');
  expect(submit.mock.calls.filter(([args]) => args[1] === 'attach')).toHaveLength(2);
  await screen.findByText('Task terminal');
  expect(h.api.terminals.mock.calls.length).toBeGreaterThan(polls);
  expect(h.api.squad.cancelRun).not.toHaveBeenCalled();
});

it('expands run history with full reason, error and unmet environment', async () => {
  const h = await show(); h.api.squad.runs.mockResolvedValueOnce([{ id: 'run_full', task_id: h.task.id, status: 'failed', started_at: '2026-11-12', finished_at: '2026-11-13', error: 'Long error detail', reason: 'Skipped prerequisite', unmet_env: ['TOKEN'] }] as never);
  taskAction('History'); const dialog = await screen.findByRole('dialog', { name: 'Run history' });
  const run = within(dialog).getByRole('button', { name: /2026-11-12/ }); fireEvent.click(run);
  expect(run.getAttribute('aria-expanded')).toBe('true');
  for (const text of ['2026-11-13', 'Long error detail', 'Skipped prerequisite', 'TOKEN']) expect(within(dialog).getByText(text)).toBeTruthy();
});
it('keeps task context and reading actions visible when mutations become read only', async () => {
  const h = harness(); render(<Squad client={h.client} available readOnly submit={vi.fn()} detach={vi.fn()} />);
  await screen.findByRole('button', { name: /Daily.*active/ }); fireEvent.click(screen.getByRole('button', { name: 'Task actions' }));
  expect((screen.getByRole('menuitem', { name: /Delete task/ }) as HTMLButtonElement).disabled).toBe(true);
  expect((screen.getByRole('menuitem', { name: 'History' }) as HTMLButtonElement).disabled).toBe(false);
});
