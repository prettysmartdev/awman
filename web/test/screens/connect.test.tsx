// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ConnectScreen } from '@/screens/connect/ConnectScreen';
import { CommandBlock } from '@/screens/components/CommandBlock';
import type { ConnectController, ConnectState } from '@/lib/connect/controller';
import { RECOVERY_COPY, type RecoveryState } from '@/lib/connect/model';
import type { BrowserHealth } from '@/lib/client/types';
import http from '../lib/fixtures/http.json';

const health = http.health_browser as BrowserHealth;
const ready = { kind: 'ready_unlinked' as const, health };
function controls() {
  const calls = { allowLocalAccess: vi.fn(), recheck: vi.fn(), connectBrowser: vi.fn(), cancelLink: vi.fn(), cancelRecovery: vi.fn(), chooseCredential: vi.fn(), forgetCredential: vi.fn() };
  return { calls, controller: calls as unknown as ConnectController };
}
function primaryButtons(container: HTMLElement) { return [...container.querySelectorAll('button:not(.secondary):not(:disabled)')]; }
afterEach(() => { cleanup(); vi.restoreAllMocks(); });
it.each(['authorization_expired', 'authorization_revoked', 'daemon_restarted', 'origin_mismatch', 'authorization_unavailable', 'listener_changed'] as RecoveryState[])('shows exact %s explanation and one linking action', state => {
  const { controller, calls } = controls();
  const { container } = render(<ConnectScreen controller={controller} state={{ phase: 'probed', result: ready, notice: { state } }} />);
  expect(screen.getByText(RECOVERY_COPY[state])).toBeTruthy();
  expect(primaryButtons(container)).toEqual([screen.getByRole('button', { name: 'Connect browser' })]);
  fireEvent.click(screen.getByRole('button', { name: 'Connect browser' }));
  expect(calls.connectBrowser).toHaveBeenCalledOnce();
});
it('cancels in-progress reauthorization without creating a link', () => {
  const { controller, calls } = controls();
  const { container } = render(<ConnectScreen controller={controller} state={{ phase: 'reauthorizing', health }} />);
  expect(screen.getByText(RECOVERY_COPY.reauthorizing)).toBeTruthy();
  expect(primaryButtons(container)).toHaveLength(0);
  fireEvent.click(screen.getByRole('button', { name: 'Cancel reconnection' }));
  expect(calls.cancelRecovery).toHaveBeenCalledOnce(); expect(calls.connectBrowser).not.toHaveBeenCalled();
});
it.each(['daemon_stopped', 'daemon_unreachable'] as const)('%s offers the right command and keeps recheck', async state => {
  const { controller, calls } = controls(); const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
  const { container } = render(<ConnectScreen controller={controller} state={{ phase: 'probed', result: { kind: 'unreachable', detail: 'network' }, notice: { state } }} />);
  expect(screen.getByText(RECOVERY_COPY[state])).toBeTruthy();
  const copy = screen.getByRole('button', { name: state === 'daemon_stopped' ? 'Copy start command' : 'Copy status command' });
  expect(primaryButtons(container)).toEqual([copy]); fireEvent.click(copy);
  await waitFor(() => expect(writeText).toHaveBeenCalledWith(state === 'daemon_stopped' ? 'awman daemon start --background' : 'awman daemon status'));
  fireEvent.click(screen.getByRole('button', { name: 'Recheck' })); expect(calls.recheck).toHaveBeenCalledOnce(); expect(calls.connectBrowser).not.toHaveBeenCalled();
});
it.each(['listener_closed', 'origin_removed', 'reauth_unsupported'] as const)('%s makes recheck primary', state => {
  const { controller } = controls();
  const { container } = render(<ConnectScreen controller={controller} state={{ phase: 'probed', result: ready, notice: { state } }} />);
  expect(screen.getByText(RECOVERY_COPY[state])).toBeTruthy(); expect(primaryButtons(container)).toEqual([screen.getByRole('button', { name: 'Recheck' })]);
});
it('labels memory-only approval and does not retry during a rate limit', () => {
  const { controller, calls } = controls();
  const { rerender, container } = render(<ConnectScreen controller={controller} state={{ phase: 'probed', result: ready, notice: { state: 'memory_only' } }} />);
  fireEvent.click(screen.getByRole('button', { name: 'Connect for this page' })); expect(calls.connectBrowser).toHaveBeenCalledOnce();
  rerender(<ConnectScreen controller={controller} state={{ phase: 'probed', result: ready, notice: { state: 'rate_limited', retryAfterSec: 12 } }} />);
  expect(screen.getByText('Automatic retry after 12 seconds.')).toBeTruthy();
  expect(primaryButtons(container)).toHaveLength(0); fireEvent.click(screen.getByRole('button', { name: 'Recheck' })); expect(calls.recheck).not.toHaveBeenCalled();
});
it('chooses or forgets only the selected saved authorization and never renders its identifier', () => {
  const { controller, calls } = controls();
  const choices = [{ credentialId: 'private-id-a', listener: 'loopback', expiresAt: '2099-01-01' }, { credentialId: 'private-id-b', listener: 'loopback', expiresAt: null }];
  const { container, rerender } = render(<ConnectScreen controller={controller} state={{ phase: 'choose_credential', health, purpose: 'recover', choices }} />);
  fireEvent.click(screen.getAllByRole('radio')[1]!);
  fireEvent.click(screen.getByRole('button', { name: 'Reconnect with selected authorization' }));
  expect(calls.chooseCredential).toHaveBeenCalledWith('private-id-b');
  fireEvent.click(screen.getByText('Manage saved access')); fireEvent.click(screen.getByRole('button', { name: 'Forget selected authorization' }));
  expect(calls.forgetCredential).toHaveBeenCalledWith('private-id-b'); expect(container.textContent).not.toContain('private-id');
  rerender(<ConnectScreen controller={controller} state={{ phase: 'choose_credential', health, purpose: 'forget', choices }} />);
  expect(primaryButtons(container)).toEqual([screen.getByRole('button', { name: 'Forget selected authorization' })]);
});
it('preserves permission, retry, cancel, missing-capability and daemon error messages', () => {
  const { controller, calls } = controls();
  const { rerender, container } = render(<ConnectScreen controller={controller} state={{ phase: 'needs_permission' }} />);
  fireEvent.click(screen.getByRole('button', { name: 'Allow local access' })); expect(calls.allowLocalAccess).toHaveBeenCalledOnce();
  rerender(<ConnectScreen controller={controller} state={{ phase: 'linking', health, status: 'creating', link: null }} />);
  fireEvent.click(screen.getByRole('button', { name: 'Cancel' })); expect(calls.cancelLink).toHaveBeenCalledOnce();
  rerender(<ConnectScreen controller={controller} state={{ phase: 'linking', health, status: 'error', link: null, error: 'engine_disabled: owner disabled engine' }} />);
  expect(screen.getByText('engine_disabled: owner disabled engine')).toBeTruthy();
  expect(primaryButtons(container)).toEqual([screen.getByRole('button', { name: 'Request a new code' })]);
  fireEvent.click(screen.getByRole('button', { name: 'Request a new code' })); expect(calls.connectBrowser).toHaveBeenCalledOnce();
  rerender(<ConnectScreen controller={controller} state={{ phase: 'probed', result: { kind: 'incompatible', health, missing: ['browser:2'] } }} />);
  expect(screen.getByText('browser:2')).toBeTruthy();
});
it('does not steal focus on a background recheck, but focuses a new step heading', () => {
  const { controller } = controls(); const state: ConnectState = { phase: 'probed', result: ready };
  const { rerender } = render(<ConnectScreen controller={controller} state={state} />);
  const button = screen.getByRole('button', { name: 'Connect browser' }); button.focus();
  rerender(<ConnectScreen controller={controller} state={{ phase: 'probing', previous: ready }} />);
  expect(document.activeElement).toBe(button);
  rerender(<ConnectScreen controller={controller} state={{ phase: 'reauthorizing', health }} />);
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1 }));
});
it('keeps clipboard failures visible and copies commands only on a gesture', async () => {
  const writeText = vi.fn().mockRejectedValue(new Error('blocked'));
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
  render(<CommandBlock commands={['awman link web-ABCD-EFGH']} label="Approval command" primary />);
  expect(writeText).not.toHaveBeenCalled(); fireEvent.click(screen.getByRole('button', { name: 'Copy approval command' }));
  await screen.findByText('Could not copy. Select the command and copy it manually.');
  writeText.mockResolvedValue(undefined); fireEvent.click(screen.getByRole('button', { name: 'Copy approval command' }));
  await screen.findByText('Approval command copied.'); expect(writeText).toHaveBeenLastCalledWith('awman link web-ABCD-EFGH');
});
