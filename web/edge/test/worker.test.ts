import { env } from 'cloudflare:workers';
import { SELF, runInDurableObject, runDurableObjectAlarm, listDurableObjectIds, reset } from 'cloudflare:test';
import { afterEach, describe, expect, it, vi } from 'vitest';
import worker, { COUNTER_NAME, COUNT_WAIT_MS, EXPORT_PATH, isNavigation } from '../src';
import { navigationDigests } from '../src/privacy';
import { expiresAt, periodsAt } from '../src/periods';
import { HLL_REGISTERS } from '../src/hll';
import type { UsageRow, UsageExport } from '../src/usage';

const origin = 'https://awman.prettysmart.dev';
const ip = '203.0.113.45';
const date = '2099-01-31T23:59:59.000Z';
function clock(at = date) { return vi.spyOn(Date, 'now').mockReturnValue(Date.parse(at)); }
function navigation(path = '/', headers: Record<string, string> = {}, method = 'GET') {
  return new Request(origin + path, { method, headers: {
    'Sec-Fetch-Mode': 'navigate', 'Sec-Fetch-Dest': 'document', Accept: 'text/html,application/xhtml+xml',
    'CF-Connecting-IP': ip, ...headers,
  } });
}
function counter() { return env.USAGE.get(env.USAGE.idFromName(COUNTER_NAME)); }
async function rows() {
  return runInDurableObject(counter(), async (_, state) => [...await state.storage.list<UsageRow>()]);
}
async function exported() { return (await counter().exportAggregates())!; }
function assertRows(result: UsageExport, total: number, missing = 0, distinct = 1) {
  expect(result.timezone).toBe('UTC');
  expect(result.distinctIpLabel).toBe('approximate distinct IPs');
  expect(result.rows).toHaveLength(2);
  for (const row of result.rows) {
    expect(row.pageLoads).toBe(total);
    expect(row.missingIpCount).toBe(missing);
    expect(row.distinctIpEstimate).toBeCloseTo(distinct, 1);
  }
}
afterEach(async () => { vi.restoreAllMocks(); await reset(); });

describe('document-only counting', () => {
  it('counts one request once, in both day and month, and protects the shell', async () => {
    clock();
    const response = await SELF.fetch(navigation());
    expect(response.status).toBe(200);
    expect(await response.text()).toContain('Shell');
    expect(response.headers.get('Cache-Control')).toBe('no-store');
    expect(response.headers.get('Referrer-Policy')).toBe('no-referrer');
    expect(response.headers.get('Strict-Transport-Security')).toBe('max-age=31536000; includeSubDomains');
    const csp = response.headers.get('Content-Security-Policy')!;
    // SR-01: the hosted document may reach only the fixed loopback daemon.
    expect(csp.split('; ').filter(directive => directive.startsWith('connect-src'))).toEqual(["connect-src 'self' http://127.0.0.1:9877 ws://127.0.0.1:9877"]);
    expect(csp).not.toMatch(/awman\.prettysmart\.dev:9877|wss:|https:\/\/[^ ;]*:9877/);
    expect(csp).toContain("frame-ancestors 'none'");
    // Only WASM compilation (the terminal core) is allowed; never JS eval/inline.
    expect(csp.split('; ')).toContain("script-src 'self' 'wasm-unsafe-eval'");
    expect(csp.replaceAll("'wasm-unsafe-eval'", '')).not.toMatch(/unsafe-inline|unsafe-eval|unsafe-hashes/);
    expect(response.headers.get('Permissions-Policy')).toContain('camera=()');
    assertRows(await exported(), 1);
    const state = await rows();
    expect(state.map(([key]) => key)).toEqual(['day:2099-01-31', 'month:2099-01']);
    for (const [, row] of state) expect(row.sketch?.length).toBe(HLL_REGISTERS);
  });

  it.each(['/app.js', '/_next/static/app.js', '/favicon.ico', '/manifest.webmanifest', '/sw.js', '/service-worker.js',
    '/api', '/api/session', '/v1/health', '/v1/engine/instances', '/health', '/healthz', '/metrics',
    '/%61pi/session', '//api/session', '/404', '/404.html', '/offline', '/__owner/nope', '/not-found'])('does not count %s', async path => {
    clock();
    await (await SELF.fetch(navigation(path))).text();
    expect(await rows()).toEqual([]);
  });

  it.each([
    { 'Sec-Fetch-Dest': 'iframe' }, { 'Sec-Fetch-Dest': 'empty' }, { 'Sec-Fetch-Mode': 'cors' },
    { Accept: 'application/json' }, { Purpose: 'prefetch' }, { 'Sec-Purpose': 'prefetch;prerender' },
    { 'Next-Router-Prefetch': '1' }, { RSC: '1' },
  ])('excludes non-initial documents %j', async headers => {
    clock();
    await (await SELF.fetch(navigation('/', headers))).text();
    expect(await rows()).toEqual([]);
  });

  it('does not count ordinary fetch, HEAD, methods, offline precache or redirects', async () => {
    clock();
    await (await SELF.fetch(origin)).text();
    await (await SELF.fetch(navigation('/', {}, 'HEAD'))).text();
    expect((await SELF.fetch(navigation('/', {}, 'POST'))).status).toBe(405);
    // The SW fetches shell assets using fetch(), with empty destination. An
    // offline cache hit performs no Worker invocation at all.
    await (await SELF.fetch(origin + '/', { headers: { Accept: 'text/html', 'Sec-Fetch-Mode': 'same-origin', 'Sec-Fetch-Dest': 'empty' } })).text();
    const redirect = await SELF.fetch(navigation('/index.html'), { redirect: 'manual' });
    expect(redirect.status).toBeGreaterThanOrEqual(300);
    expect(redirect.status).toBeLessThan(400);
    expect(await rows()).toEqual([]);
    await (await SELF.fetch(navigation('/'))).text();
    assertRows(await exported(), 1);
  });

  it('rejects API-like paths even if an asset fallback would return HTML', () => {
    for (const path of ['/api', '/api/x', '/v1', '/v1/x', '/v2/status', '/health', '/metrics', '/manifest', '/service-worker', '/assets/foo', '/_next/foo']) {
      expect(isNavigation(navigation(path), path)).toBe(false);
    }
  });

  it('ignores X-Forwarded-For and counts missing/invalid connecting IPs', async () => {
    clock();
    const missing = navigation('/', { 'X-Forwarded-For': ip });
    missing.headers.delete('CF-Connecting-IP');
    await (await SELF.fetch(missing)).text();
    await (await SELF.fetch(navigation('/', { 'CF-Connecting-IP': 'invalid IP', 'X-Forwarded-For': ip }))).text();
    assertRows(await exported(), 2, 2, 0);
    await (await SELF.fetch(navigation())).text();
    assertRows(await exported(), 3, 2, 1);
  });

  it('deduplicates canonical IPv6 spellings and keeps the full address', async () => {
    clock();
    for (const address of ['2001:db8::a', '2001:0DB8:0:0:0:0:0:000a', '2001:db8::b']) {
      await (await SELF.fetch(navigation('/', { 'CF-Connecting-IP': address }))).text();
    }
    assertRows(await exported(), 3, 0, 2);
  });

  it('does not lose concurrent increments and deduplicates each of ten IPs', async () => {
    clock();
    const responses = await Promise.all(Array.from({ length: 100 }, (_, i) => SELF.fetch(navigation('/', { 'CF-Connecting-IP': `192.0.2.${i % 10}` }))));
    for (const response of responses) {
      expect(response.status).toBe(200);
      await response.text();
    }
    assertRows(await exported(), 100, 0, 10);
  });
});

describe('period boundaries, atomicity and retention', () => {
  it('keeps separate days/months and deletes closed sketches at midnight', async () => {
    const time = clock('2099-01-30T23:59:59Z');
    await (await SELF.fetch(navigation())).text();
    time.mockReturnValue(Date.parse('2099-01-31T00:00:01Z'));
    await (await SELF.fetch(navigation())).text();
    let stored = new Map(await rows());
    expect(stored.get('day:2099-01-30')?.sketch).toBeUndefined();
    expect(stored.get('day:2099-01-31')?.pageLoads).toBe(1);
    expect(stored.get('month:2099-01')?.pageLoads).toBe(2);
    expect(stored.get('month:2099-01')?.distinctIpEstimate).toBeCloseTo(1, 2);
    time.mockReturnValue(Date.parse('2099-02-01T00:00:01Z'));
    expect(await runDurableObjectAlarm(counter())).toBe(true);
    stored = new Map(await rows());
    expect(stored.get('day:2099-01-31')?.sketch).toBeUndefined();
    expect(stored.get('month:2099-01')?.sketch).toBeUndefined();
    await (await SELF.fetch(navigation())).text();
    const summary = await exported();
    expect(summary.rows).toHaveLength(5);
    stored = new Map(await rows());
    expect(stored.get('month:2099-02')?.pageLoads).toBe(1);
    expect(stored.get('month:2099-02')?.distinctIpEstimate).toBeCloseTo(1, 2);
  });

  it('rejects stale periods without incrementing or reopening a sketch', async () => {
    clock('2099-02-01T00:00:00Z');
    const result = await counter().record({
      periods: { day: '2099-01-31', month: '2099-01' },
      digests: { day: new Uint8Array(32), month: new Uint8Array(32) },
    });
    expect(result).toBe('period_changed');
    expect(await rows()).toEqual([]);
  });

  it('resamples across midnight without double-counting the stale attempt', async () => {
    const time = clock();
    const stub = counter();
    let calls = 0;
    await runInDurableObject(stub, instance => {
      const record = instance.record.bind(instance);
      vi.spyOn(Object.getPrototypeOf(instance) as typeof instance, 'record').mockImplementation(input => {
        if (calls++ === 0) time.mockReturnValue(Date.parse('2099-02-01T00:00:00Z'));
        return record(input);
      });
    });
    const response = await worker.fetch(navigation(), env);
    expect(response.status).toBe(200);
    expect(calls).toBe(2);
    assertRows(await exported(), 1);
    expect((await rows()).map(([key]) => key)).toEqual(['day:2099-02-01', 'month:2099-02']);
  });

  it.each([
    ['unavailable', () => Promise.resolve('unavailable' as const)],
    ['throwing', () => Promise.reject(new Error('sensitive-fault-marker'))],
    ['hanging', () => new Promise<never>(() => undefined)],
  ])('serves the page when the counter is %s (fail open, privacy review #1)', async (_name, outcome) => {
    clock();
    await runInDurableObject(counter(), instance => {
      vi.spyOn(Object.getPrototypeOf(instance) as typeof instance, 'record').mockImplementation(outcome as never);
    });
    const started = Date.now();
    const response = await worker.fetch(navigation(), env);
    expect(response.status).toBe(200);
    expect(await response.text()).toContain('Shell');
    expect(response.headers.get('Content-Security-Policy')).toContain("frame-ancestors 'none'");
    expect(Date.now() - started).toBeLessThan(COUNT_WAIT_MS + 2_000);
    expect(await rows()).toEqual([]);
  });

  it('rolls back the whole transaction if the second row update fails', async () => {
    clock();
    await (await SELF.fetch(navigation())).text();
    const before = await rows();
    const digests = await navigationDigests(ip, env.USAGE_HMAC_SECRET, periodsAt(Date.now()));
    await runInDurableObject(counter(), async (instance, state) => {
      const original = state.storage.transaction.bind(state.storage);
      const spy = vi.spyOn(state.storage, 'transaction').mockImplementation(callback => original(async txn => {
        const put = txn.put.bind(txn);
        vi.spyOn(txn, 'put').mockImplementation(async (...args: Parameters<typeof txn.put>) => {
          if ((args[0] as unknown) === 'month:2099-01') throw new Error('sensitive-fault-marker');
          return put(...args);
        });
        return callback(txn);
      }));
      try {
        expect(await instance.record({ periods: periodsAt(Date.now()), digests })).toBe('unavailable');
      } finally { spy.mockRestore(); }
    });
    expect(await rows()).toEqual(before);
  });

  it('expires rows after 13 calendar months from period end without traffic', async () => {
    const time = clock();
    await (await SELF.fetch(navigation())).text();
    const expiry = Date.parse('2100-03-01T00:00:00Z');
    expect(expiresAt('day', '2099-01-31')).toBe(expiry);
    expect(expiresAt('month', '2099-01')).toBe(expiry);
    time.mockReturnValue(expiry - 1);
    await runDurableObjectAlarm(counter());
    expect(await rows()).toHaveLength(2);
    expect((await rows()).every(([, row]) => !row.sketch)).toBe(true);
    time.mockReturnValue(expiry);
    await runDurableObjectAlarm(counter());
    expect(await rows()).toEqual([]);
    expect(await runInDurableObject(counter(), (_, state) => state.storage.getAlarm())).toBeNull();
    // Jan 31 end dates clamp rather than rolling over in a short February.
    expect(expiresAt('day', '2098-01-30')).toBe(Date.parse('2099-02-28T00:00:00Z'));
  });
});

describe('owner boundary and privacy', () => {
  it('requires the separate owner bearer secret, rejects URLs/browser origins, exports only aggregate fields', async () => {
    clock();
    await (await SELF.fetch(navigation())).text();
    const auth = { Authorization: `Bearer ${env.OWNER_EXPORT_SECRET}` };
    for (const headers of [{}, { Authorization: 'Bearer wrong' }, { Authorization: `Bearer ${env.USAGE_HMAC_SECRET}` }, { ...auth, Origin: origin }]) {
      expect((await SELF.fetch(origin + EXPORT_PATH, { headers })).status).toBe(403);
    }
    expect((await SELF.fetch(origin + EXPORT_PATH + '?token=ignored', { headers: auth })).status).toBe(403);
    expect((await SELF.fetch(origin + EXPORT_PATH, { method: 'POST', headers: auth })).status).toBe(403);
    // Plain HTTP never honours the owner bearer (privacy review #3).
    expect((await SELF.fetch(origin.replace('https:', 'http:') + EXPORT_PATH, { headers: auth })).status).toBe(403);
    const response = await SELF.fetch(origin + EXPORT_PATH, { headers: auth });
    expect(response.status).toBe(200);
    expect(response.headers.get('Cache-Control')).toBe('no-store');
    expect(response.headers.has('Access-Control-Allow-Origin')).toBe(false);
    const summary = await response.json<UsageExport>();
    assertRows(summary, 1);
    for (const row of summary.rows) expect(Object.keys(row).sort()).toEqual(['distinctIpEstimate', 'kind', 'missingIpCount', 'pageLoads', 'period']);
    assertRows(await exported(), 1);
  });

  it('persists no IP, digest, per-visitor key or credential; exports and errors stay clean', async () => {
    clock();
    const addresses = [ip, '2001:db8::a'];
    const forbidden: string[] = [...addresses, '2001:0db8:0000:0000:0000:0000:0000:000a', env.USAGE_HMAC_SECRET, env.OWNER_EXPORT_SECRET];
    for (const address of addresses) {
      const hashes = await navigationDigests(address, env.USAGE_HMAC_SECRET, periodsAt(Date.now()));
      for (const digest of [hashes.day!, hashes.month!]) {
        forbidden.push([...digest].map(x => x.toString(16).padStart(2, '0')).join(''), JSON.stringify([...digest]), JSON.stringify(digest));
      }
      const response = await SELF.fetch(navigation('/?ignored=sensitive-query', { 'CF-Connecting-IP': address }));
      const body = await response.text();
      for (const value of forbidden) expect(body).not.toContain(value);
    }
    const stored = await rows();
    expect(stored).toHaveLength(2);
    for (const [key, row] of stored) {
      expect(key).toMatch(/^(day:\d{4}-\d{2}-\d{2}|month:\d{4}-\d{2})$/);
      expect(Object.keys(row).sort()).toEqual(['distinctIpEstimate', 'kind', 'missingIpCount', 'pageLoads', 'period', 'sketch']);
      expect(row.sketch).toBeInstanceOf(Uint8Array);
      expect(row.sketch!.length).toBe(HLL_REGISTERS);
    }
    expect(await listDurableObjectIds(env.USAGE)).toHaveLength(1);
    const persisted = JSON.stringify(stored);
    const summary = JSON.stringify(await exported());
    // A misconfigured counter never takes the site down: the page is served
    // (fail open) and nothing is counted or leaked.
    const failure = await worker.fetch(navigation(), { ...env, USAGE_HMAC_SECRET: '' });
    expect(failure.status).toBe(200);
    expect(failure.headers.get('Content-Security-Policy')).toContain("default-src 'none'");
    const error = await failure.text();
    expect(error).toContain('Shell');
    expect(JSON.stringify(await rows())).toBe(persisted);
    for (const value of forbidden) {
      expect(persisted).not.toContain(value);
      expect(summary).not.toContain(value);
      expect(error).not.toContain(value);
    }
  });

  it('strips sensitive headers and query strings before calling the static binding', async () => {
    clock();
    const staticFetch = vi.spyOn(env.ASSETS, 'fetch').mockImplementation(async request => {
      const incoming = request as Request;
      expect(new URL(incoming.url).search).toBe('');
      for (const name of ['CF-Connecting-IP', 'X-Forwarded-For', 'Authorization', 'Cookie']) expect(incoming.headers.has(name)).toBe(false);
      return new Response('Shell', { headers: { 'Content-Type': 'text/html' } });
    });
    const result = await worker.fetch(navigation('/?secret=private', { Cookie: 'private', Authorization: 'Bearer private' }), env);
    expect(result.status).toBe(200);
    expect(staticFetch).toHaveBeenCalledOnce();
  });
});
