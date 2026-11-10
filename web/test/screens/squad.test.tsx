// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: () => <div>Task terminal</div> }));
import { Squad } from '@/components/Squad';
import { harness } from './helpers';
afterEach(cleanup);
async function show() { const h = harness(); const submit = vi.fn(); render(<Squad client={h.client} available={true} readOnly={false} submit={submit} detach={vi.fn()} />); await screen.findByRole('button', { name: /Daily.*active/ }); return { ...h, submit }; }
it('shows unavailable recipes for missing listener features', () => { const h = harness(); render(<Squad client={h.client} available={false} readOnly={false} submit={vi.fn()} detach={vi.fn()} />); expect(screen.getByText('awman config set daemon.network.listeners.loopback.features engine,squad')).toBeTruthy(); expect(screen.getByText('awman config set daemon.features.squad.enabled true')).toBeTruthy(); expect(h.api.squad.tasks).not.toHaveBeenCalled(); });
it('renders cards, details, history and inactive command reminder', async () => { const h = await show(); expect(screen.getByText(/command box is inactive/)).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Details' })); await screen.findByRole('dialog', { name: 'Daily' }); expect(h.api.squad.task).toHaveBeenCalledWith(h.task.id); fireEvent.click(screen.getByRole('button', { name: 'Close' })); fireEvent.click(screen.getByRole('button', { name: 'History' })); await screen.findByRole('dialog', { name: 'Run history' }); expect(h.api.squad.runs).toHaveBeenCalledWith(h.task.id); });
it.each([['Pause', 'patch'], ['Trigger', 'trigger'], ['Cancel run', 'cancelRun']] as const)('requires engine prompt before %s', async (label, method) => { const h = await show(); fireEvent.click(screen.getByRole('button', { name: label })); await screen.findByText('Engine wording for squad_task_confirm'); expect(h.api.squad[method]).not.toHaveBeenCalled(); fireEvent.click(screen.getByRole('button', { name: 'Confirm task action' })); await waitFor(() => expect(h.api.squad[method]).toHaveBeenCalledTimes(1)); });
it('new and edit actions use the engine interview; attach uses argv with no shell interpolation', async () => { const h = await show(); fireEvent.click(screen.getByRole('button', { name: 'New task' })); await waitFor(() => expect(h.submit).toHaveBeenCalledWith(['squad', 'add', '--interview'])); fireEvent.click(screen.getByRole('button', { name: 'Edit' })); await waitFor(() => expect(h.submit).toHaveBeenCalledWith(['squad', 'edit', h.task.name, '--interview'])); fireEvent.click(screen.getByRole('button', { name: 'Attach' })); await waitFor(() => expect(h.api.terminals).toHaveBeenCalledWith(h.task.id)); expect(h.submit).toHaveBeenCalledWith(['squad', 'attach', h.task.name]); });
it('delete confirmation can be declined and confirmed', async () => { const h = await show(); fireEvent.click(screen.getByRole('button', { name: 'Delete' })); const dialog = await screen.findByRole('dialog', { name: 'Delete task' }); fireEvent.click(dialog.querySelectorAll('button')[1]!); expect(h.api.squad.delete).not.toHaveBeenCalled(); fireEvent.click(screen.getByRole('button', { name: 'Delete' })); const next = await screen.findByRole('dialog', { name: 'Delete task' }); fireEvent.click(next.querySelectorAll('button')[0]!); await waitFor(() => expect(h.api.squad.delete).toHaveBeenCalledWith(h.task.id)); });
it('displays actionable route errors without optimistic task changes', async () => { const h = await show(); h.api.squad.patch.mockRejectedValueOnce(new Error('conflict: task changed')); fireEvent.click(screen.getByRole('button', { name: 'Pause' })); fireEvent.click(await screen.findByRole('button', { name: 'Confirm task action' })); await screen.findByText('conflict: task changed'); expect(screen.getByRole('button', { name: /Daily.*active/ })).toBeTruthy(); });
it('detach asks before canceling the attach and leaves the task running', async () => { const h = harness(); const detach = vi.fn(); render(<Squad client={h.client} available={true} readOnly={false} submit={vi.fn()} detach={detach} />); await screen.findByRole('button', { name: /Daily.*active/ }); fireEvent.click(screen.getByRole('button', { name: 'Attach' })); await screen.findByText(/Waiting for the task terminal/); fireEvent.keyDown(document, { key: 'c', ctrlKey: true }); await screen.findByRole('dialog', { name: 'Detach from task?' }); expect(detach).not.toHaveBeenCalled(); fireEvent.click(screen.getByRole('dialog').querySelector('button')!); expect(detach).toHaveBeenCalledTimes(1); expect(h.api.squad.cancelRun).not.toHaveBeenCalled(); });
it('visible Detach button clears the attachment so the same task can be re-attached (review #5)', async () => {
  const h = harness(); const detach = vi.fn(); const submit = vi.fn(); const onAttachmentChange = vi.fn();
  (h.api.terminals as unknown as ReturnType<typeof vi.fn>).mockResolvedValue([{ id: 'term_task', state: 'open' }]);
  render(<Squad client={h.client} available={true} readOnly={false} submit={submit} detach={detach} onAttachmentChange={onAttachmentChange} />);
  await screen.findByRole('button', { name: /Daily.*active/ });
  fireEvent.click(screen.getByRole('button', { name: 'Attach' }));
  await screen.findByText('Task terminal');
  expect(onAttachmentChange).toHaveBeenLastCalledWith(true);
  fireEvent.click(screen.getByRole('button', { name: 'Detach' }));
  fireEvent.click(within(await screen.findByRole('dialog', { name: 'Detach from task?' })).getByRole('button', { name: 'Detach' }));
  expect(detach).toHaveBeenCalledTimes(1);
  expect(screen.queryByText('Task terminal')).toBeNull();
  expect(screen.queryByText(/Waiting for the task terminal/)).toBeNull();
  expect(onAttachmentChange).toHaveBeenLastCalledWith(false);
  const polls = h.api.terminals.mock.calls.length;
  fireEvent.click(screen.getByRole('button', { name: 'Attach' }));
  expect(submit.mock.calls.filter(([args]) => args[1] === 'attach')).toHaveLength(2);
  await screen.findByText('Task terminal');
  expect(h.api.terminals.mock.calls.length).toBeGreaterThan(polls);
  expect(h.api.squad.cancelRun).not.toHaveBeenCalled();
});
