import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { LinkDenied, LinkExpired, LinkGone } from '@/lib/client/errors';
import { clearGrant, currentGrant } from '@/lib/client/grant';
import { createLink, linkCommand, pollLink, type LinkRequest } from '@/lib/client/links';
import { fakeFetch, fixture } from './helpers';

const http = fixture<Record<string, unknown>>('http.json');
const noSleep = async () => undefined;
const REQUEST: LinkRequest = {
  id: 'blk_0123456789abcdef0123456789abcdef',
  code: 'web-K7PM-3Q9X',
  pollingSecret: 'S'.repeat(43),
  intervalMs: 2000,
  expiresAt: '2999-01-01T00:00:00Z',
  listener: 'loopback',
  origin: 'https://awman.prettysmart.dev',
};

// Fixture timestamps are fixed (the approved grant expires 2026-11-09T21:15:40Z),
// so every test runs on a controlled clock rather than the wall clock.
const FIXTURE_NOW = Date.parse('2026-11-09T09:15:40.000Z');
beforeEach(() => {
  vi.useFakeTimers({ toFake: ['Date'] });
  vi.setSystemTime(FIXTURE_NOW);
});
afterEach(() => {
  clearGrant();
  vi.useRealTimers();
});

describe('browser links', () => {
  it('creates a link with an empty JSON body and exposes the approval command', async () => {
    const { calls, transport } = fakeFetch(() => ({ status: 201, body: http.link_created }));
    const link = await createLink(transport);
    expect(calls[0]).toMatchObject({ url: 'http://127.0.0.1:9877/v1/browser-links', init: { method: 'POST', body: '{}' } });
    expect(link.code).toMatch(/^web-[A-Z0-9]{4}-[A-Z0-9]{4}$/);
    expect(linkCommand(link)).toBe(`awman link ${link.code}`);
  });

  it('polls with the secret in the body until approved, then holds the grant in memory', async () => {
    let n = 0;
    const { calls, transport } = fakeFetch(() => (n++ < 2 ? { status: 200, body: http.link_poll_pending } : { status: 200, body: http.link_poll_approved }));
    const waits: number[] = [];
    const grant = await pollLink(REQUEST, new AbortController().signal, { transport, sleep: async ms => void waits.push(ms) });
    expect(calls).toHaveLength(3);
    for (const call of calls) {
      expect(call.url).toBe(`http://127.0.0.1:9877/v1/browser-links/${REQUEST.id}/poll`);
      expect(call.url).not.toContain(REQUEST.pollingSecret);
      expect(JSON.parse(String(call.init.body))).toEqual({ polling_secret: REQUEST.pollingSecret });
    }
    expect(waits.every(ms => ms >= 2000)).toBe(true);
    expect(currentGrant()).toEqual(grant);
    expect(grant.token).toMatch(/^awb_/);
  });

  it('forgets an approved grant once its expiry passes (controlled clock)', async () => {
    const { transport } = fakeFetch(() => ({ status: 200, body: http.link_poll_approved }));
    const grant = await pollLink(REQUEST, new AbortController().signal, { transport, sleep: noSleep });
    const expires = Date.parse(grant.expiresAt);
    expect(currentGrant()).toEqual(grant);
    vi.setSystemTime(expires - 1);
    expect(currentGrant()).toEqual(grant);
    vi.setSystemTime(expires);
    expect(currentGrant()).toBeNull();
  });

  it('reports denial, gone and expiry distinctly', async () => {
    await expect(pollLink(REQUEST, new AbortController().signal, { transport: fakeFetch(() => ({ status: 200, body: http.link_poll_denied })).transport, sleep: noSleep })).rejects.toBeInstanceOf(LinkDenied);
    await expect(pollLink(REQUEST, new AbortController().signal, { transport: fakeFetch(() => ({ status: 410, body: { error: 'gone', code: 'link_gone' } })).transport, sleep: noSleep })).rejects.toBeInstanceOf(LinkGone);
    const expired = { ...REQUEST, expiresAt: '2000-01-01T00:00:00Z' };
    await expect(pollLink(expired, new AbortController().signal, { transport: fakeFetch(() => ({ status: 200, body: http.link_poll_pending })).transport, sleep: noSleep })).rejects.toBeInstanceOf(LinkExpired);
    expect(currentGrant()).toBeNull();
  });

  it('backs off on 429 using Retry-After without consuming the link', async () => {
    let n = 0;
    const { transport } = fakeFetch(() => (n++ === 0 ? { status: 429, body: { error: 'slow', code: 'rate_limited' }, headers: { 'Retry-After': '5' } } : { status: 200, body: http.link_poll_approved }));
    const waits: number[] = [];
    await pollLink(REQUEST, new AbortController().signal, { transport, sleep: async ms => void waits.push(ms) });
    expect(waits).toEqual([2000, 5000]);
  });

  it('stops when aborted', async () => {
    const abort = new AbortController();
    abort.abort();
    await expect(pollLink(REQUEST, abort.signal, { transport: fakeFetch(() => ({ status: 200, body: http.link_poll_pending })).transport })).rejects.toBeDefined();
  });
});
