import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { cacheNameFor, decide, isSkipWaiting, staleCaches } from '@/sw/policy';

const SELF = 'https://awman.prettysmart.dev';
const PRECACHE = new Set(['/', '/404.html', '/_next/static/chunks/a.js', '/manifest.webmanifest']);

describe('service worker fetch policy', () => {
  it('never intercepts the daemon (different origin)', () => {
    for (const url of ['http://127.0.0.1:9877/v1/health', 'ws://127.0.0.1:9877/v1/terminals/x', 'http://127.0.0.1:9877/']) {
      expect(decide({ url, method: 'GET', mode: 'cors' }, SELF, PRECACHE)).toEqual({ kind: 'ignore' });
      expect(decide({ url, method: 'GET', mode: 'navigate' }, SELF, PRECACHE)).toEqual({ kind: 'ignore' });
    }
  });

  it('never answers same-origin /v1/* (auth routes included), Authorization or POST requests (WI 0160 C8)', () => {
    const withV1 = new Set([...PRECACHE, '/v1/browser-reauth/challenge', '/v1/health']);
    for (const path of ['/v1/browser-reauth/challenge', '/v1/browser-reauth/complete', '/v1/browser-links', '/v1/health', '/V1/health']) {
      for (const mode of ['cors', 'navigate', 'no-cors', 'same-origin']) {
        expect(decide({ url: `${SELF}${path}`, method: 'GET', mode }, SELF, withV1)).toEqual({ kind: 'ignore' });
        expect(decide({ url: `${SELF}${path}`, method: 'POST', mode }, SELF, withV1)).toEqual({ kind: 'ignore' });
      }
    }
    expect(decide({ url: `${SELF}/_next/static/chunks/a.js`, method: 'GET', mode: 'cors', hasAuthorization: true }, SELF, PRECACHE)).toEqual({ kind: 'ignore' });
    expect(decide({ url: `${SELF}/`, method: 'GET', mode: 'navigate', hasAuthorization: true }, SELF, PRECACHE)).toEqual({ kind: 'ignore' });
  });

  it('the worker source has no auth broker: no IndexedDB, keys, signing, sync or session messages', () => {
    const source = readFileSync(join(__dirname, '../../src/sw/sw.ts'), 'utf8') + readFileSync(join(__dirname, '../../src/sw/policy.ts'), 'utf8');
    const code = source.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/.*$/gm, '');
    expect(code).not.toMatch(/indexedDB|crypto\.subtle|\bsync\b|periodicsync|BroadcastChannel|clients\.matchAll|postMessage|awb_|Authorization: /);
  });

  it('serves precached shell assets cache-first', () => {
    expect(decide({ url: `${SELF}/_next/static/chunks/a.js`, method: 'GET', mode: 'no-cors' }, SELF, PRECACHE)).toEqual({ kind: 'cache-first', key: '/_next/static/chunks/a.js' });
  });

  it('answers in-scope navigations with the cached shell document (offline launch)', () => {
    expect(decide({ url: `${SELF}/`, method: 'GET', mode: 'navigate' }, SELF, PRECACHE)).toEqual({ kind: 'cache-first', key: '/' });
    expect(decide({ url: `${SELF}/anything/`, method: 'GET', mode: 'navigate' }, SELF, PRECACHE)).toEqual({ kind: 'cache-first', key: '/' });
  });

  it('ignores non-GET, query strings, unknown paths and API-like/owner paths', () => {
    expect(decide({ url: `${SELF}/`, method: 'POST', mode: 'navigate' }, SELF, PRECACHE).kind).toBe('ignore');
    expect(decide({ url: `${SELF}/_next/static/chunks/a.js?x=1`, method: 'GET', mode: 'cors' }, SELF, PRECACHE).kind).toBe('ignore');
    expect(decide({ url: `${SELF}/_next/static/chunks/other.js`, method: 'GET', mode: 'cors' }, SELF, PRECACHE).kind).toBe('ignore');
    expect(decide({ url: `${SELF}/__owner/usage`, method: 'GET', mode: 'navigate' }, SELF, PRECACHE).kind).toBe('ignore');
    expect(decide({ url: `${SELF}/v1/health`, method: 'GET', mode: 'navigate' }, SELF, PRECACHE).kind).toBe('ignore');
    expect(decide({ url: 'not a url', method: 'GET', mode: 'cors' }, SELF, PRECACHE).kind).toBe('ignore');
  });

  it('does not serve navigations before the shell is cached', () => {
    expect(decide({ url: `${SELF}/`, method: 'GET', mode: 'navigate' }, SELF, new Set()).kind).toBe('ignore');
  });
});

describe('service worker cache lifecycle', () => {
  it('deletes only older awman shell caches', () => {
    expect(staleCaches(['awman-shell-old', 'awman-shell-new', 'other-app'], 'new')).toEqual(['awman-shell-old']);
    expect(cacheNameFor('abc')).toBe('awman-shell-abc');
  });

  it('accepts only the SKIP_WAITING message', () => {
    expect(isSkipWaiting({ type: 'SKIP_WAITING' })).toBe(true);
    expect(isSkipWaiting({ type: 'CACHE', url: 'http://127.0.0.1:9877/' })).toBe(false);
    expect(isSkipWaiting(null)).toBe(false);
  });
});
