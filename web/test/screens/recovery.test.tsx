// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { WorkspacePicker } from '@/components/WorkspacePicker';
import { FrontendConnection, type CloseReason } from '@/lib/client/frontendSocket';
import { InstanceStore } from '@/lib/client/instanceStore';
import { Emitter } from '@/lib/client/observable';
import { RECOVERY_COPY } from '@/lib/client/recovery';
import { readResume, writeResume } from '@/lib/state/session';
import { harness } from './helpers';
vi.mock('@/lib/client/frontendSocket', () => ({ FrontendConnection: vi.fn() }));
afterEach(() => { cleanup(); sessionStorage.clear(); vi.resetAllMocks(); });
function setup() {
  const h = harness(); const closes = new Emitter<CloseReason>(); const store = new InstanceStore();
  h.connection.onClose = callback => closes.on(callback);
  Object.assign(h.connection, { store }); h.state.set('connecting');
  vi.mocked(FrontendConnection).mockImplementation(function () { return h.connection; });
  writeResume({ instanceId: 'ins_resume', frontendId: 'fe_resume', tabId: 'tab_resume', launchContext: 'lc_default' });
  const open = vi.fn(); const rendered = render(<WorkspacePicker client={h.client} onOpen={open} />);
  return { ...h, ...rendered, closes, store, open };
}
it('holds the resume hint and opening connection until a fresh snapshot arrives', async () => {
  const h = setup();
  expect(screen.getByRole('heading', { name: 'Reconnecting to your workspace…' })).toBeTruthy();
  expect(readResume()?.frontendId).toBe('fe_resume'); expect(h.open).not.toHaveBeenCalled(); expect(h.api.createInstance).not.toHaveBeenCalled();
  act(() => { h.state.set('open'); h.store.applySnapshot(h.s); });
  await waitFor(() => expect(h.open).toHaveBeenCalledWith(h.connection, 'lc_default'));
  h.unmount(); expect(h.connection.close).not.toHaveBeenCalled(); expect(h.send).not.toHaveBeenCalled();
});
it('another primary offers read-only attachment without a new instance or resume frame', () => {
  const h = setup(); act(() => h.closes.emit({ kind: 'other_primary' }));
  expect(screen.getByText(RECOVERY_COPY.other_primary)).toBeTruthy();
  expect(h.container.querySelectorAll('button:not(.secondary)')).toHaveLength(1);
  fireEvent.click(screen.getByRole('button', { name: 'Attach read only' }));
  expect(FrontendConnection).toHaveBeenLastCalledWith(h.client, 'ins_resume', { role: 'secondary', termSize: { cols: 100, rows: 30 } });
  expect(h.api.createInstance).not.toHaveBeenCalled(); expect(h.send).not.toHaveBeenCalled(); expect(readResume()).not.toBeNull();
});
it('manual resume is explicit, preserves ids, and a refusal is displayed verbatim', () => {
  const h = setup(); act(() => h.closes.emit({ kind: 'other_primary' }));
  fireEvent.click(screen.getByRole('button', { name: 'Resume workspace' }));
  expect(FrontendConnection).toHaveBeenLastCalledWith(h.client, 'ins_resume', { role: 'primary', resumeFrontendId: 'fe_resume', manualResume: true, termSize: { cols: 100, rows: 30 } });
  act(() => h.closes.emit({ kind: 'refused', code: 'primary_exists', error: 'primary still active' }));
  expect(screen.getByText('primary_exists: primary still active')).toBeTruthy(); expect(FrontendConnection).toHaveBeenCalledTimes(2);
});
it.each(['not_found', 'grace_expired'] as const)('%s chooses another workspace without relinking', async kind => {
  const h = setup(); act(() => h.closes.emit({ kind }));
  expect(screen.getByText(RECOVERY_COPY.resume_unavailable)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Choose another workspace' }));
  await screen.findByRole('option', { name: /Project/ });
  expect(readResume()).toBeNull(); expect(h.connection.close).toHaveBeenCalledWith('navigation'); expect(h.api.createInstance).not.toHaveBeenCalled();
});
it('leaving automatic resume closes its pending connection and allows workspace selection', async () => {
  const h = setup(); fireEvent.click(screen.getByRole('button', { name: 'Choose another workspace' }));
  await screen.findByRole('option', { name: /Project/ });
  act(() => { h.state.set('open'); h.store.applySnapshot(h.s); });
  expect(h.open).not.toHaveBeenCalled(); expect(h.connection.close).toHaveBeenCalledWith('navigation');
});
