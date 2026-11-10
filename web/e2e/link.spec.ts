// Contract-driven browser link tests. Missing WI 0159 browser support is a
// failure, never mistaken for an unavailable optional WI 0143 capability.
import type { Page } from '@playwright/test';
import { test, expect } from './fixtures.js';
import { endpoint, origin } from './daemon.js';

async function request(page: Page, path: string, body?: unknown, token?: string) {
  return page.evaluate(async ({ endpoint, path, body, token }) => {
    const response = await fetch(endpoint + path, {
      method: body === undefined ? 'GET' : 'POST', cache: 'no-store',
      headers: { ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
        ...(token ? { Authorization: `Bearer ${token}` } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, cache: response.headers.get('Cache-Control'), body: await response.json() };
  }, { endpoint, path, body, token });
}

async function create(page: Page) {
  await page.evaluate(endpoint => {
    document.querySelector('button')!.addEventListener('click', () => {
      // Drain the fetch body so Chromium emits loadingFinished for the
      // Playwright response reader even when the fixture discards the value.
      void fetch(endpoint + '/v1/browser-links', {
        method: 'POST', cache: 'no-store', headers: { 'Content-Type': 'application/json' }, body: '{}',
      }).then(response => response.json());
    }, { once: true });
  }, endpoint);
  const [response] = await Promise.all([
    page.waitForResponse(endpoint + '/v1/browser-links'), page.getByRole('button', { name: 'Probe' }).click(),
  ]);
  expect(response.status()).toBe(201);
  expect(response.headers()['cache-control']).toContain('no-store');
  return await response.json() as { id: string; code: string; polling_secret: string; interval_ms: number; expires_at: string };
}

test.beforeEach(async ({ page, daemon }) => {
  await daemon.allowOrigin();
  await daemon.recipe();
  await daemon.start();
  await page.goto('/__transport_fixture');
  const health = await request(page, '/v1/health');
  expect(health.status).toBe(200);
  expect(health.body.browser_links, 'WI 0159 browser-link implementation is required').toBe(true);
  expect(health.body.protocols.browser).toBe(1);
});

test('explicit owner confirmation, launch context, scope and revocation', async ({ page, daemon }) => {
  const link = await create(page);
  const output = await daemon.confirmLink(link.code, true);
  expect(output).toContain(origin);
  expect(output).toContain('loopback');
  expect(output).toContain('engine');
  await page.waitForTimeout(link.interval_ms);
  const poll = await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret });
  expect(poll.body.status).toBe('approved');
  expect(poll.cache).toContain('no-store');
  const grant = poll.body.grant;
  expect(grant.scope).toBe('engine');
  expect(grant.origin).toBe(origin);
  expect(grant.listener).toBe('loopback');
  expect((await request(page, '/v1/engine/launch_contexts', undefined, grant.token)).status).toBe(200);
  expect((await request(page, '/v1/worker/workdirs', undefined, grant.token)).status).toBe(404);
  await page.waitForTimeout(link.interval_ms);
  expect((await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret })).status).toBe(410);
  await daemon.cli('daemon', 'api', 'browsers.delete', `id=${grant.id}`);
  expect((await request(page, '/v1/status', undefined, grant.token)).status).toBe(401);
});

test('owner denial consumes the link without a grant', async ({ page, daemon }) => {
  const link = await create(page);
  await daemon.confirmLink(link.code, false);
  await page.waitForTimeout(link.interval_ms);
  expect((await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret })).body.status).toBe('denied');
  await page.waitForTimeout(link.interval_ms);
  expect((await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret })).status).toBe(410);
});

test('expired codes cannot be polled into a grant', async ({ page }) => {
  test.setTimeout(360_000); // Real five-minute expiry; no production clock bypass.
  const link = await create(page);
  const remaining = Date.parse(link.expires_at) - Date.now();
  expect(remaining).toBeGreaterThan(0);
  expect(remaining).toBeLessThanOrEqual(300_000);
  await page.waitForTimeout(remaining + 1000);
  expect((await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret })).status).toBe(410);
});
