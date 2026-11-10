import { afterEach, describe, expect, it } from 'vitest';
import { LinkDenied, LinkExpired } from '@/lib/client/errors';
import { REQUIRED_CAPABILITIES, type ProbeResult } from '@/lib/client/health';
import { clearGrant, setGrant } from '@/lib/client/grant';
import type { LinkRequest } from '@/lib/client/links';
import type { BrowserHealth } from '@/lib/client/types';
import { ConnectController, REPROBE_INTERVAL_MS, type ConnectDeps } from '@/lib/connect/controller';
import { connectView } from '@/lib/connect/model';
import { DAEMON_START, ENABLE_RECIPE } from '@/lib/connect/recipe';
import { GRANT, flush } from './helpers';

const HEALTH: BrowserHealth = {
  status: 'ok',
  build: '0.159.0',
  protocols: { api: 1, frontend: 1, terminal: 1, browser: 1 },
  capabilities: [...REQUIRED_CAPABILITIES],
  listener: { name: 'loopback', transport: 'loopback_http', features: ['engine'] },
  engine: { state: 'on' },
  browser_links: true,
};
const LINK: LinkRequest = { id: 'blk_1', code: 'web-K7PM-3Q9X', pollingSecret: 'S'.repeat(43), intervalMs: 2000, expiresAt: '2999-01-01T00:00:00Z', listener: 'loopback', origin: 'http://localhost:3000' };

afterEach(() => clearGrant());

function controller(results: ProbeResult[], extra: Partial<ConnectDeps> = {}) {
  const timers: { fn: () => void; ms: number }[] = [];
  let probes = 0;
  const c = new ConnectController({
    probe: async () => results[Math.min(probes++, results.length - 1)]!,
    permission: async () => 'unknown',
    pageIsLoopback: () => false,
    setTimer: (fn, ms) => timers.push({ fn, ms }),
    clearTimer: () => timers.splice(0),
    ...extra,
  });
  return { c, timers, probeCount: () => probes };
}

describe('connect view model: distinct states', () => {
  const view = (result: ProbeResult) => connectView({ phase: 'probed', result });

  it('unreachable: two-line recipe + daemon start + status hint, never "stopped"', () => {
    const v = view({ kind: 'unreachable', detail: 'network' });
    expect(v.recipe).toEqual([...ENABLE_RECIPE]);
    expect(v.recipe).toEqual(['awman config set daemon.features.engine.enabled true', 'awman config set daemon.network.listeners.loopback.enabled true']);
    expect(v.startCommand).toBe(DAEMON_START);
    expect(v.statusHint).toMatch(/awman daemon status/);
    expect(v.statusHint).toMatch(/different port or failed to bind/);
    expect([v.title, ...v.body].join(' ')).not.toMatch(/\bis stopped\b|\bnot running\b/i);
    expect(v.actions).toEqual(['recheck']);
  });

  it('engine off: recipe but no daemon start (the listener answered)', () => {
    const v = view({ kind: 'engine_off', health: { ...HEALTH, engine: { state: 'off' } }, reason: 'engine_off' });
    expect(v.recipe).toEqual([...ENABLE_RECIPE]);
    expect(v.startCommand).toBeNull();
  });

  it('incompatible daemon: update panel naming what is missing, no recipe', () => {
    const v = view({ kind: 'incompatible', health: HEALTH, missing: ['web_frontend'] });
    expect(v.kind).toBe('incompatible');
    expect(v.missing).toEqual(['web_frontend']);
    expect(v.recipe).toBeNull();
  });

  it('missing grant: Connect browser', () => {
    expect(view({ kind: 'ready_unlinked', health: HEALTH }).actions).toEqual(['connect', 'recheck']);
  });

  it('denied permission, blocked by browser and unsupported browser are separate states', () => {
    expect(view({ kind: 'permission_denied' }).kind).toBe('permission_denied');
    expect(view({ kind: 'unreachable', detail: 'blocked_by_browser' }).kind).toBe('blocked_by_browser');
    expect(view({ kind: 'unsupported', missing: ['WebSocket'] }).kind).toBe('unsupported');
    expect(connectView({ phase: 'needs_permission' }).actions).toEqual(['allow']);
  });

  it('linking shows the code and the copyable approval command', () => {
    const v = connectView({ phase: 'linking', health: HEALTH, status: 'waiting', link: LINK });
    expect(v.link).toEqual({ code: 'web-K7PM-3Q9X', command: 'awman link web-K7PM-3Q9X', expiresAt: LINK.expiresAt });
  });
});

describe('ConnectController', () => {
  it('asks for a user gesture before probing when the browser will prompt', async () => {
    const { c, probeCount } = controller([{ kind: 'ready_unlinked', health: HEALTH }], { permission: async () => 'prompt' });
    await c.start();
    expect(c.state.get()).toEqual({ phase: 'needs_permission' });
    expect(probeCount()).toBe(0);
    await c.allowLocalAccess();
    expect(c.state.get()).toMatchObject({ phase: 'probed', result: { kind: 'ready_unlinked' } });
  });

  it('skips the gesture when the page itself is on loopback (dev/e2e)', async () => {
    const { c } = controller([{ kind: 'unreachable', detail: 'network' }], { permission: async () => 'prompt', pageIsLoopback: () => true });
    await c.start();
    expect(c.state.get()).toMatchObject({ phase: 'probed', result: { kind: 'unreachable' } });
  });

  it('re-probes every 3 s while on the connect screen, and Recheck probes now', async () => {
    const { c, timers, probeCount } = controller([{ kind: 'unreachable', detail: 'network' }, { kind: 'engine_off', health: HEALTH, reason: 'engine_off' }, { kind: 'ready_unlinked', health: HEALTH }]);
    await c.start();
    expect(timers.at(-1)?.ms).toBe(REPROBE_INTERVAL_MS);
    timers.at(-1)?.fn();
    await flush();
    expect(c.state.get()).toMatchObject({ phase: 'probed', result: { kind: 'engine_off' } });
    await c.recheck();
    expect(c.state.get()).toMatchObject({ phase: 'probed', result: { kind: 'ready_unlinked' } });
    expect(probeCount()).toBe(3);
  });

  it('links the browser, then connects with the in-memory grant', async () => {
    const { c } = controller([{ kind: 'ready_unlinked', health: HEALTH }, { kind: 'ready', health: HEALTH }], {
      createLink: async () => LINK,
      pollLink: async () => {
        setGrant(GRANT);
        return GRANT;
      },
    });
    await c.start();
    await c.connectBrowser();
    expect(c.state.get()).toMatchObject({ phase: 'connected', grant: GRANT });
  });

  it('shows the code while waiting, and denial/expiry as distinct outcomes', async () => {
    let release: (value: never) => void = () => undefined;
    const waiting = controller([{ kind: 'ready_unlinked', health: HEALTH }], {
      createLink: async () => LINK,
      pollLink: () => new Promise<never>((_resolve, reject) => (release = reject as (value: never) => void)),
    });
    await waiting.c.start();
    const linking = waiting.c.connectBrowser();
    await flush();
    expect(waiting.c.state.get()).toMatchObject({ phase: 'linking', status: 'waiting', link: LINK });
    release(new LinkDenied() as never);
    await linking;
    expect(waiting.c.state.get()).toMatchObject({ phase: 'linking', status: 'denied' });

    const expired = controller([{ kind: 'ready_unlinked', health: HEALTH }], { createLink: async () => LINK, pollLink: async () => { throw new LinkExpired(); } });
    await expired.c.start();
    await expired.c.connectBrowser();
    expect(expired.c.state.get()).toMatchObject({ phase: 'linking', status: 'expired' });
  });

  it('does not re-probe while waiting for approval', async () => {
    const { c, probeCount } = controller([{ kind: 'ready_unlinked', health: HEALTH }], { createLink: async () => LINK, pollLink: () => new Promise<never>(() => undefined) });
    await c.start();
    void c.connectBrowser();
    await flush();
    await c.recheck();
    expect(probeCount()).toBe(1);
    expect(c.state.get()).toMatchObject({ phase: 'linking', status: 'waiting' });
  });

  // Review #4: Cancel must leave the link flow, re-probe, and allow a new link.
  const abortable = <T,>(signal: AbortSignal) => new Promise<T>((_resolve, reject) => signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError'))));
  it.each(['creating', 'waiting'] as const)('Cancel while %s returns to the probe result and a later link succeeds', async stage => {
    let attempt = 0;
    const { c, probeCount } = controller([{ kind: 'ready_unlinked', health: HEALTH }, { kind: 'ready_unlinked', health: HEALTH }, { kind: 'ready', health: HEALTH }], {
      createLink: signal => (stage === 'creating' && attempt === 0 ? abortable<LinkRequest>(signal!) : Promise.resolve(LINK)),
      pollLink: (_link, signal) => {
        if (attempt++ === 0) return abortable(signal);
        setGrant(GRANT);
        return Promise.resolve(GRANT);
      },
    });
    await c.start();
    const first = c.connectBrowser();
    await flush();
    expect(c.state.get()).toMatchObject({ phase: 'linking', status: stage });
    c.cancelLink();
    await first;
    await flush();
    expect(probeCount()).toBe(2);
    expect(c.state.get()).toMatchObject({ phase: 'probed', result: { kind: 'ready_unlinked' } });
    await c.recheck();
    expect(probeCount()).toBe(3);
    if (stage === 'creating') attempt = 1;
    await c.connectBrowser();
    expect(c.state.get()).toMatchObject({ phase: 'connected', grant: GRANT });
  });
});
