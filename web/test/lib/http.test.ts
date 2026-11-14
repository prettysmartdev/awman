import { afterEach, describe, expect, it, vi } from 'vitest';
import { DaemonClient } from '@/lib/client/daemonClient';
import { DaemonError, DaemonUnreachable, GrantRevoked, ProtocolError } from '@/lib/client/errors';
import { clearGrant, currentGrant, setGrant } from '@/lib/client/grant';
import { daemonEndpoints, HttpTransport } from '@/lib/client/http';
import { GRANT, TICKET, fakeFetch, fixture } from './helpers';

const http = fixture<Record<string, unknown>>('http.json');

afterEach(() => clearGrant());

describe('HttpTransport', () => {
  // SR-01: the destination is the fixed same-machine listener (WI 0159 §2/§4),
  // never derived from the page's host or protocol.
  const FIXED = { http: 'http://127.0.0.1:9877', ws: 'ws://127.0.0.1:9877', hostPort: '127.0.0.1:9877' };
  it.each([
    'https://awman.prettysmart.dev/', 'https://awman.prettysmart.dev:443/app?x=1', 'http://127.0.0.1:3000/', 'http://localhost:3000/',
    'http://100.101.102.103:3000/', 'https://host.tailnet.example/',
  ])('targets only http/ws://127.0.0.1:9877 from page %s', pageUrl => {
    const page = new URL(pageUrl);
    const endpoints = daemonEndpoints(page);
    expect(endpoints).toEqual(FIXED);
    // No remote-protocol destination: no TLS variant, no page host on the daemon port.
    expect(`${endpoints.http} ${endpoints.ws}`).not.toMatch(/https:|wss:/);
    if (page.hostname !== '127.0.0.1') expect(`${endpoints.http} ${endpoints.ws}`).not.toContain(page.hostname);
  });

  it('a module loaded by the hosted page sends every request to loopback with the loopback hint', async () => {
    vi.resetModules();
    vi.stubGlobal('location', new URL('https://awman.prettysmart.dev/'));
    try {
      const hosted = await import('@/lib/client/http');
      expect([hosted.DAEMON_HTTP, hosted.DAEMON_WS, hosted.DAEMON_HOST_PORT]).toEqual([FIXED.http, FIXED.ws, FIXED.hostPort]);
      const calls: { url: string; init: RequestInit }[] = [];
      const transport = new hosted.HttpTransport(async (url, init) => {
        calls.push({ url, init });
        return new Response('{"status":"ok"}', { status: 200 });
      });
      await transport.json('/v1/health');
      expect(calls[0]!.url).toBe('http://127.0.0.1:9877/v1/health');
      expect(calls[0]!.init).toMatchObject({ targetAddressSpace: 'loopback', credentials: 'omit', redirect: 'error' });
    } finally {
      vi.unstubAllGlobals();
      vi.resetModules();
    }
  });
  it('sends every request no-store, credential-less, redirect-refusing, with the loopback hint', async () => {
    const { calls, transport } = fakeFetch(() => ({ status: 200, body: { status: 'ok' } }));
    await transport.json('/v1/health');
    const [call] = calls;
    expect(call?.url).toBe('http://127.0.0.1:9877/v1/health');
    expect(call?.init).toMatchObject({ cache: 'no-store', credentials: 'omit', redirect: 'error', mode: 'cors', referrerPolicy: 'no-referrer', targetAddressSpace: 'loopback' });
  });

  it('puts the grant only in the Authorization header', async () => {
    const { calls, transport } = fakeFetch(() => ({ status: 200, body: http.status }));
    await new DaemonClient(GRANT, transport).status();
    const call = calls[0]!;
    expect((call.init.headers as Record<string, string>).Authorization).toBe(`Bearer ${GRANT.token}`);
    expect(call.url).not.toContain(GRANT.token);
  });

  it('surfaces {error, code} as a typed DaemonError with Retry-After', async () => {
    const { transport } = fakeFetch(() => ({ status: 429, body: { error: 'slow down', code: 'rate_limited' }, headers: { 'Retry-After': '2' } }));
    const error = await transport.json('/v1/browser-links', { method: 'POST', body: {} }).catch(e => e);
    expect(error).toBeInstanceOf(DaemonError);
    expect(error).toMatchObject({ status: 429, code: 'rate_limited', message: 'slow down', retryAfterSec: 2 });
  });

  it('maps network failures to DaemonUnreachable (never "stopped")', async () => {
    const { transport } = fakeFetch(() => new TypeError('Failed to fetch'));
    await expect(transport.json('/v1/health')).rejects.toMatchObject({ name: 'DaemonUnreachable', detail: 'network' });
    const blocked = fakeFetch(() => new TypeError('Permission was denied for this request to access the `loopback` address space.'));
    await expect(blocked.transport.json('/v1/health')).rejects.toBeInstanceOf(DaemonUnreachable);
    await expect(blocked.transport.json('/v1/health')).rejects.toMatchObject({ detail: 'blocked_by_browser' });
  });

  it('refuses redirects, malformed JSON, oversized bodies and non-API paths', async () => {
    await expect(fakeFetch(() => ({ status: 302, headers: { Location: 'https://evil.example/' } })).transport.json('/v1/health')).rejects.toBeInstanceOf(ProtocolError);
    const malformed = new HttpTransport(async () => new Response('{nope', { status: 200 }));
    await expect(malformed.json('/v1/health')).rejects.toBeInstanceOf(ProtocolError);
    const { transport } = fakeFetch(() => ({ status: 200, body: {} }));
    await expect(transport.json('/v1/x', { method: 'POST', body: { big: 'x'.repeat(70 * 1024) } })).rejects.toThrow(/64 KiB/);
    await expect(transport.json('https://evil.example/')).rejects.toBeInstanceOf(ProtocolError);
  });
});

describe('DaemonClient', () => {
  it('clears the in-memory grant and fires onRevoked once on 401 browser_grant_invalid', async () => {
    setGrant(GRANT);
    const { transport } = fakeFetch(() => ({ status: 401, body: { error: 'revoked', code: 'browser_grant_invalid' } }));
    const client = new DaemonClient(GRANT, transport);
    let fired = 0;
    client.onRevoked(() => (fired += 1));
    await expect(client.status()).rejects.toBeInstanceOf(GrantRevoked);
    await expect(client.status()).rejects.toBeInstanceOf(GrantRevoked);
    expect(fired).toBe(1);
    expect(currentGrant()).toBeNull();
  });

  it('requests tickets with the documented body and validates the ticket', async () => {
    const { calls, transport } = fakeFetch(() => ({ status: 201, body: { ticket: TICKET, expires_in_ms: 30000 } }));
    const client = new DaemonClient(GRANT, transport);
    expect(await client.ticket('frontend', 'ins_1')).toBe(TICKET);
    expect(JSON.parse(String(calls[0]!.init.body))).toEqual({ route: 'frontend', instance_id: 'ins_1' });
    expect(await client.ticket('terminal', 'term_1')).toBe(TICKET);
    expect(JSON.parse(String(calls[1]!.init.body))).toEqual({ route: 'terminal', terminal_id: 'term_1' });
    const bad = fakeFetch(() => ({ status: 201, body: { ticket: 'awt_short', expires_in_ms: 1 } }));
    await expect(new DaemonClient(GRANT, bad.transport).ticket('frontend', 'ins_1')).rejects.toBeInstanceOf(DaemonError);
  });

  it('caches the catalogue by ETag and revalidates with If-None-Match', async () => {
    let n = 0;
    const { calls, transport } = fakeFetch(() => (n++ === 0 ? { status: 200, body: http.catalogue } : { status: 304 }));
    const client = new DaemonClient(GRANT, transport);
    const first = await client.catalogue();
    const second = await client.catalogue();
    expect(second).toBe(first);
    expect((calls[1]!.init.headers as Record<string, string>)['If-None-Match']).toBe(`"${(http.catalogue as { etag: string }).etag}"`);
  });

  it('lists only web-primary instances', async () => {
    const list = { items: [
      { id: 'ins_web', primary_frontend_kind: 'web', annotations: {}, tab_count: 1, running_jobs: 0, created_at: '2026-01-01T00:00:00Z' },
      { id: 'ins_tui', primary_frontend_kind: 'tui', annotations: {}, tab_count: 1, running_jobs: 0, created_at: '2026-01-01T00:00:00Z' },
    ], next_cursor: null };
    const { transport } = fakeFetch(() => ({ status: 200, body: list }));
    expect((await new DaemonClient(GRANT, transport).listInstances()).map(i => i.id)).toEqual(['ins_web']);
  });

  it('builds prompt and squad requests', async () => {
    const { calls, transport } = fakeFetch(url =>
      url.includes('prompts') ? { status: 200, body: http.prompt_squad_task_confirm } : { status: 202, body: { id: 'run_1' } },
    );
    const client = new DaemonClient(GRANT, transport);
    await client.prompt('squad_task_confirm', { action: 'pause', name: 'nightly' });
    expect(calls[0]!.url).toBe('http://127.0.0.1:9877/v1/engine/prompts?kind=squad_task_confirm&action=pause&name=nightly');
    await client.squad.trigger('task_1');
    expect(calls[1]).toMatchObject({ url: 'http://127.0.0.1:9877/v1/squad/tasks/task_1/runs', init: { method: 'POST' } });
  });
});

describe('bounded response bodies (review #8)', () => {
  it('cancels a chunked body without Content-Length as soon as it exceeds the bound', async () => {
    const { readBounded, MAX_RESPONSE_BYTES } = await import('@/lib/client/http');
    let pulled = 0; let cancelled = false;
    const chunk = new Uint8Array(1024 * 1024).fill(0x61);
    const body = new ReadableStream<Uint8Array>({
      pull(controller) { pulled++; controller.enqueue(chunk); },
      cancel() { cancelled = true; },
    });
    await expect(readBounded(new Response(body))).rejects.toThrow(/too large/);
    expect(cancelled).toBe(true);
    expect(pulled).toBeLessThanOrEqual(MAX_RESPONSE_BYTES / chunk.length + 2);
  });

  it('reads a normal chunked body, joining UTF-8 across chunks', async () => {
    const { readBounded } = await import('@/lib/client/http');
    const parts = [new Uint8Array([0x7b, 0x22, 0xe2, 0x82]), new Uint8Array([0xac, 0x22, 0x3a, 0x31, 0x7d])];
    const body = new ReadableStream<Uint8Array>({ start(controller) { for (const p of parts) controller.enqueue(p); controller.close(); } });
    expect(JSON.parse(await readBounded(new Response(body)))).toEqual({ '€': 1 });
  });
});
