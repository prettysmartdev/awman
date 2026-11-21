// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { REQUIRED_CAPABILITIES } from '@/lib/client/health';
import type { BrowserHealth } from '@/lib/client/types';
import { ConnectController, type ConnectState } from '@/lib/connect/controller';
import { ConnectScreen } from '@/screens/connect/ConnectScreen';

const HEALTH: BrowserHealth = {
  status: 'ok',
  build: '0.159.0',
  protocols: { api: 1, frontend: 1, terminal: 1, browser: 1 },
  capabilities: [...REQUIRED_CAPABILITIES],
  listener: { name: 'loopback', transport: 'loopback_http', features: ['engine'] },
  engine: { state: 'on' },
  browser_links: true,
};

afterEach(cleanup);

const show = (state: ConnectState) => render(<ConnectScreen state={state} controller={new ConnectController({ setTimer: () => 0, clearTimer: () => undefined })} />);

describe('ConnectScreen accessible names (web/e2e/app.spec.ts)', () => {
  it('unreachable: recipe lines as text, daemon start, Recheck', () => {
    show({ phase: 'probed', result: { kind: 'unreachable', detail: 'network' } });
    expect(screen.getByText('awman config set daemon.features.engine.enabled true')).toBeTruthy();
    expect(screen.getByText('awman config set daemon.network.listeners.loopback.enabled true')).toBeTruthy();
    expect(screen.getByText('awman daemon start --background')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Recheck' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Connect browser' })).toBeNull();
  });

  it('permission gesture: Allow local access', () => {
    show({ phase: 'needs_permission' });
    expect(screen.getByRole('button', { name: 'Allow local access' })).toBeTruthy();
  });

  it('ready without a grant: Connect browser', () => {
    show({ phase: 'probed', result: { kind: 'ready_unlinked', health: HEALTH } });
    expect(screen.getByRole('button', { name: 'Connect browser' })).toBeTruthy();
  });

  it('waiting for approval: the awman link command is visible text', () => {
    show({
      phase: 'linking',
      health: HEALTH,
      status: 'waiting',
      link: { id: 'blk_1', code: 'web-K7PM-3Q9X', pollingSecret: 's'.repeat(43), intervalMs: 2000, expiresAt: '2999-01-01T00:00:00Z', listener: 'loopback', origin: 'x' },
    });
    expect(screen.getByText(/awman link web-[A-Z0-9]{4}-[A-Z0-9]{4}/)).toBeTruthy();
    expect(document.body.textContent).not.toContain('s'.repeat(43));
  });

  it('incompatible: names the missing capability and offers no recipe', () => {
    show({ phase: 'probed', result: { kind: 'incompatible', health: HEALTH, missing: ['web_frontend'] } });
    expect(screen.getByText('web_frontend')).toBeTruthy();
    expect(screen.queryByText(/awman config set/)).toBeNull();
  });
});
