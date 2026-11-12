// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueuePanel } from '@/components/QueuePanel';
import { harness } from './helpers';
import type { PickRun } from '@/lib/client/types';

afterEach(cleanup);

it('presents the instance queue and hands a claimed run to the engine tab flow', async () => {
  const h = harness();
  const run: PickRun = { lease_id: 'lease_1', title: 'Fix CI', server: 'team', cwd: '/repo/lease', command_line: 'exec workflow --dynamic', argv: ['exec', 'workflow', '--dynamic'] };
  h.queueRequest.mockImplementation(async (_tab, request) => {
    if (request.operation === 'snapshot') return { result: 'snapshot', value: { items: [{ id: 'item_1', server: 'team', title: 'Fix CI', summary: 'Repair checks', kind: 'task', labels: { pool: 'laptop' }, priority: 2, age_ms: 120_000, for_you: true, body: 'Details', source_url: null }], for_you: 1, new_since_seen: 1, error: null, loaded: true } };
    if (request.operation === 'pick') return { result: 'picked', value: run };
    if (request.operation === 'checkout_offer') return { result: 'checkout_offer', value: false };
    return { result: 'done' };
  });
  const onPick = vi.fn(async () => undefined);
  const onClose = vi.fn();
  render(<QueuePanel connection={h.connection} tabId="tab_1" readOnly={false} onPick={onPick} onClose={onClose} />);
  fireEvent.click(await screen.findByRole('button', { name: 'Fix CI · team' }));
  expect(screen.getByText('Details')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Pick' }));
  await waitFor(() => expect(onPick).toHaveBeenCalledWith(run));
  expect(h.queueRequest).toHaveBeenCalledWith('tab_1', { operation: 'pick', server: 'team', item_id: 'item_1' });
  expect(onClose).toHaveBeenCalled();
});

it('offers the WI143 checkout choice and releases an unstarted claim on dismiss', async () => {
  const h = harness();
  const run: PickRun = { lease_id: 'lease_2', title: 'Review', server: 'team', cwd: '/clone', command_line: 'exec workflow --dynamic', argv: ['exec', 'workflow', '--dynamic'] };
  h.queueRequest.mockImplementation(async (_tab, request) => {
    if (request.operation === 'snapshot') return { result: 'snapshot', value: { items: [{ id: 'item_2', server: 'team', title: 'Review', summary: null, kind: 'task', labels: {}, priority: 1, age_ms: 0, for_you: false, body: null, source_url: null }], for_you: 0, new_since_seen: 0, error: null, loaded: true } };
    if (request.operation === 'pick') return { result: 'picked', value: run };
    if (request.operation === 'checkout_offer') return { result: 'checkout_offer', value: true };
    if (request.operation === 'checkout') return { result: 'picked', value: { ...run, cwd: '/checkout/worktree' } };
    return { result: 'done' };
  });
  const onPick = vi.fn(async () => undefined);
  const onClose = vi.fn();
  const view = render(<QueuePanel connection={h.connection} tabId="tab_1" readOnly={false} onPick={onPick} onClose={onClose} />);
  fireEvent.click(await screen.findByRole('button', { name: 'Review · team' }));
  fireEvent.click(screen.getByRole('button', { name: 'Pick' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Run in a worktree of this checkout' }));
  await waitFor(() => expect(onPick).toHaveBeenCalledWith({ ...run, cwd: '/checkout/worktree' }));
  expect(h.queueRequest).toHaveBeenCalledWith('tab_1', { operation: 'checkout', lease_id: 'lease_2', choice: 'worktree' });
  view.unmount();
  render(<QueuePanel connection={h.connection} tabId="tab_1" readOnly={false} onPick={onPick} onClose={onClose} />);
  fireEvent.click(await screen.findByRole('button', { name: 'Review · team' }));
  fireEvent.click(screen.getByRole('button', { name: 'Pick' }));
  await screen.findByRole('button', { name: 'Run in a fresh clone' });
  act(() => h.state.set('reconnecting'));
  for (const name of ['Run in a worktree of this checkout', 'Run in a fresh clone']) expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled).toBe(true);
  const closes = onClose.mock.calls.length; fireEvent.click(screen.getByRole('button', { name: 'Close' }));
  expect(onClose).toHaveBeenCalledTimes(closes); expect(screen.getByRole('alert').textContent).toContain('Reconnect as primary');
  act(() => h.state.set('open'));
  fireEvent.click(screen.getByRole('button', { name: 'Close' }));
  await waitFor(() => expect(h.queueRequest).toHaveBeenCalledWith('tab_1', { operation: 'finish', lease_id: 'lease_2', choice: 'release' }));
});
