// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { useSyncExternalStore } from 'react';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { Subject } from '@/lib/client/observable';
import type { ConnectController, ConnectState } from '@/lib/connect/controller';
import type { FrontendConnection } from '@/lib/client/frontendSocket';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { BrowserHealth } from '@/lib/client/types';
import http from '../lib/fixtures/http.json';
import { harness } from './helpers';
import { RECOVERY_COPY, RECOVERY_TITLES } from '@/lib/connect/model';
import type { AuthEndState } from '@/lib/client/recovery';
import App from '@/app/App';
const captured = vi.hoisted(() => ({ state: null as Subject<ConnectState> | null, controller: null as ConnectController | null, client: null as DaemonClient | null, connection: null as FrontendConnection | null, revoked: null as ((state: AuthEndState) => void) | null }));
vi.mock('@/lib/connect/useConnect', () => ({ useConnect: () => ({ state: useSyncExternalStore(cb => captured.state!.subscribe(cb), () => captured.state!.get()), controller: captured.controller }) }));
vi.mock('@/lib/client/daemonClient', () => ({ DaemonClient: vi.fn(function () { return captured.client; }) }));
vi.mock('@/lib/client/frontendSocket', () => ({ FrontendConnection: vi.fn(function () { return captured.connection; }) }));
vi.mock('@/lib/pwa/serviceWorker', () => ({ registerShellWorker: () => () => undefined }));
vi.mock('@/components/TerminalPanel', () => ({ TerminalPanel: () => <div>Hub terminal</div> }));
afterEach(() => { cleanup(); sessionStorage.clear(); });
it('an authorization ending returns to connect with its precise recovery copy and closes the workspace socket (H3)', async () => {
  const h = harness(); const health = http.health_browser as BrowserHealth;
  captured.client = h.client; captured.connection = h.connection;
  captured.state = new Subject<ConnectState>({ phase: 'connected', grant: { id: 'brw_test', token: 'synthetic-grant', scope: 'engine', origin: '', listener: 'loopback', expiresAt: '2099-01-01' }, health });
  captured.controller = { reset: () => captured.state!.set({ phase: 'probing', previous: null }) } as ConnectController;
  h.client.onAuthEnded = cb => { captured.revoked = cb; return () => { captured.revoked = null; }; };
  render(<App />); fireEvent.click(await screen.findByRole('button', { name: 'Open workspace' }));
  await screen.findByRole('textbox', { name: 'Command' });
  act(() => captured.revoked?.('authorization_expired'));
  // The precise state, never a blanket "revoked".
  expect(screen.getByRole('heading', { name: RECOVERY_TITLES.authorization_expired })).toBeTruthy();
  expect(screen.getByText(RECOVERY_COPY.authorization_expired)).toBeTruthy();
  expect(screen.queryByText(/revoked/i)).toBeNull();
  // Once the connect flow settles it explains the current state itself.
  act(() => captured.state!.set({ phase: 'probed', result: { kind: 'ready_unlinked', health }, notice: null }));
  expect(screen.queryByText(RECOVERY_COPY.authorization_expired)).toBeNull();
  expect(screen.getByRole('button', { name: 'Connect browser' })).toBeTruthy();
  await waitFor(() => expect(h.connection.close).toHaveBeenCalledWith('navigation'));
});
