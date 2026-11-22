// Real browser -> real loopback daemon. These tests cover the listener only;
// application behavior is separately gated in app.spec.ts.
import { test, expect } from './fixtures.js';
import { endpoint } from './daemon.js';

test('default-off, stopped, live recipe and idempotent recheck', async ({ page, daemon }) => {
  await page.goto('/__transport_fixture');
  const probe = () => page.evaluate(async endpoint => {
    try { return (await fetch(endpoint + '/v1/health', { cache: 'no-store' })).status; }
    catch { return null; }
  }, endpoint);
  expect(await probe()).toBeNull();
  await daemon.start();
  const before = await daemon.status();
  expect(before.listeners.network.default?.bound ?? null).toBeNull();
  expect(before.listeners.network.loopback?.bound ?? null).toBeNull();
  expect(await probe()).toBeNull();
  for (let i = 0; i < 2; i++) {
    await daemon.recipe();
    await expect.poll(probe).toBe(200);
    const after = await daemon.status();
    expect(after.listeners.network.default).toEqual(before.listeners.network.default);
  }
  await daemon.stop();
  await expect.poll(probe).toBeNull();
});

test('exact origin, replacement and browser requests cannot use API keys', async ({ page, daemon }) => {
  await daemon.recipe();
  await daemon.start();
  await page.goto('/__transport_fixture');
  const probe = () => page.evaluate(async endpoint => {
    try { return (await fetch(endpoint + '/v1/health', { cache: 'no-store' })).status; }
    catch { return null; }
  }, endpoint);
  // Both local development origins are present in the default listener policy.
  await expect.poll(probe).toBe(200);
  const key = JSON.parse(await daemon.cli('daemon', 'api', 'keys.create', 'name=browser-rejection', '--json')).key;
  const status = await page.evaluate(async ({ endpoint, key }) => {
    return (await fetch(endpoint + '/v1/status', {
      cache: 'no-store', headers: { Authorization: `Bearer ${key}` },
    })).status;
  }, { endpoint, key });
  expect(status).toBe(401);
  await daemon.set('daemon.network.listeners.loopback.allowedOrigins', 'http://localhost:3000');
  await expect.poll(probe).toBeNull();
});
