import { describe, expect, it } from 'vitest';
import { REQUIRED_CAPABILITIES, classifyHealth, loopbackPermission, missingBrowserApis, probe } from '@/lib/client/health';
import type { BrowserHealth } from '@/lib/client/types';
import { fakeFetch } from './helpers';

const READY: BrowserHealth = {
  status: 'ok',
  build: '0.159.0',
  protocols: { api: 1, frontend: 1, terminal: 1, browser: 1 },
  capabilities: [...REQUIRED_CAPABILITIES, 'health'],
  listener: { name: 'loopback', transport: 'loopback_http', features: ['engine'] },
  engine: { state: 'on' },
  browser_links: true,
};

const deps = (handler: Parameters<typeof fakeFetch>[0], extra: Partial<Parameters<typeof probe>[1]> = {}) => ({
  transport: fakeFetch(handler).transport,
  permission: async () => 'unknown' as const,
  missingApis: () => [],
  hasLiveGrant: () => false,
  ...extra,
});

describe('classifyHealth', () => {
  it('distinguishes ready, unlinked, engine off, draining, listener without engine and incompatible', () => {
    expect(classifyHealth(READY, true).kind).toBe('ready');
    expect(classifyHealth(READY, false).kind).toBe('ready_unlinked');
    expect(classifyHealth({ ...READY, engine: { state: 'off' } }, false)).toMatchObject({ kind: 'engine_off', reason: 'engine_off' });
    expect(classifyHealth({ ...READY, engine: { state: 'draining' } }, false)).toMatchObject({ kind: 'engine_off', reason: 'engine_draining' });
    expect(classifyHealth({ ...READY, listener: { ...READY.listener, features: ['worker'] } }, false)).toMatchObject({ kind: 'engine_off', reason: 'listener_without_engine' });
    expect(classifyHealth({ ...READY, engine: { state: 'degraded' } }, false).kind).toBe('ready_unlinked');
  });

  it('reports exactly which capabilities or protocols are missing', () => {
    const old = { ...READY, capabilities: READY.capabilities.filter(c => c !== 'web_frontend' && c !== 'browser_tickets') };
    expect(classifyHealth(old, true)).toMatchObject({ kind: 'incompatible', missing: ['browser_tickets', 'web_frontend'] });
    const newer = { ...READY, protocols: { ...READY.protocols, frontend: 2 } };
    expect(classifyHealth(newer, true)).toMatchObject({ kind: 'incompatible', missing: ['protocol frontend v1'] });
    const noBrowserProtocol = { ...READY, protocols: { api: 1, frontend: 1, terminal: 1 } };
    expect(classifyHealth(noBrowserProtocol, true)).toMatchObject({ kind: 'incompatible', missing: ['protocol browser v1'] });
  });

  it('an incompatible daemon wins over engine off (the recipe would not help)', () => {
    expect(classifyHealth({ ...READY, capabilities: [], engine: { state: 'off' } }, false).kind).toBe('incompatible');
  });
});

describe('probe()', () => {
  it('classifies an unreachable endpoint without claiming the daemon stopped', async () => {
    expect(await probe(undefined, deps(() => new TypeError('Failed to fetch')))).toEqual({ kind: 'unreachable', detail: 'network' });
  });

  it('reports a denied Local Network Access permission (before and after the fetch)', async () => {
    expect(await probe(undefined, deps(() => ({ status: 200, body: READY }), { permission: async () => 'denied' }))).toEqual({ kind: 'permission_denied' });
    let asked = 0;
    const flips = deps(() => new TypeError('Failed to fetch'), { permission: async () => (asked++ === 0 ? 'prompt' : 'denied') });
    expect(await probe(undefined, flips)).toEqual({ kind: 'permission_denied' });
  });

  it('reports an unsupported browser before probing', async () => {
    expect(await probe(undefined, deps(() => ({ status: 200, body: READY }), { missingApis: () => ['WebSocket'] }))).toEqual({ kind: 'unsupported', missing: ['WebSocket'] });
  });

  it('treats a non-browser health body or an error status as incompatible', async () => {
    expect((await probe(undefined, deps(() => ({ status: 200, body: { status: 'ok' } })))).kind).toBe('incompatible');
    expect((await probe(undefined, deps(() => ({ status: 404, body: { error: 'nope', code: 'not_found' } })))).kind).toBe('incompatible');
  });

  it('returns ready only with a live in-memory grant', async () => {
    expect((await probe(undefined, deps(() => ({ status: 200, body: READY })))).kind).toBe('ready_unlinked');
    expect((await probe(undefined, deps(() => ({ status: 200, body: READY }), { hasLiveGrant: () => true }))).kind).toBe('ready');
  });
});

describe('browser capability helpers', () => {
  it('reads the loopback permission under either name and tolerates unknown names', async () => {
    const perms = { query: async ({ name }: { name: string }) => { if (name === 'loopback-network') throw new TypeError('unknown'); return { state: 'granted' }; } };
    expect(await loopbackPermission(perms)).toBe('granted');
    expect(await loopbackPermission(undefined)).toBe('unknown');
    expect(await loopbackPermission({ query: async () => { throw new TypeError('x'); } })).toBe('unknown');
  });

  it('lists missing browser APIs', () => {
    expect(missingBrowserApis({ fetch: 1, WebSocket: 1, AbortController: 1, TextEncoder: 1, TextDecoder: 1, atob: 1, btoa: 1, crypto: 1 })).toEqual([]);
    expect(missingBrowserApis({ fetch: 1 })).toContain('WebSocket');
  });
});
