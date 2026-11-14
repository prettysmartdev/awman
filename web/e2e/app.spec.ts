import { access } from 'node:fs/promises';
import { resolve } from 'node:path';
import { test, expect } from './fixtures.js';
import { root } from './daemon.js';

test.beforeAll(async () => {
  for (const file of ['web/package.json', 'web/out/index.html', 'web/edge/package.json']) {
    await access(resolve(root, file)).catch(() => {
      throw new Error(`WI 0159 acceptance blocked: required upstream artifact ${file} is absent. This is not a WI 0143 skip.`);
    });
  }
});

for (const state of ['stopped', 'engine off', 'listener off'] as const) {
  test(`setup recipe with ${state}, then recheck`, async ({ page, daemon }) => {
    await daemon.allowOrigin();
    if (state === 'engine off') await daemon.set('daemon.network.listeners.loopback.enabled', 'true');
    if (state === 'listener off') await daemon.set('daemon.features.engine.enabled', 'true');
    if (state !== 'stopped') await daemon.start();
    await page.goto('/');
    const permission = page.getByRole('button', { name: 'Allow local access', exact: true });
    if (await permission.isVisible()) await permission.click();
    await expect(page.getByText('awman config set daemon.features.engine.enabled true', { exact: false })).toBeVisible();
    await expect(page.getByText('awman config set daemon.network.listeners.loopback.enabled true', { exact: false })).toBeVisible();
    await daemon.recipe();
    if (state === 'stopped') await daemon.start();
    await page.getByRole('button', { name: 'Recheck', exact: true }).click();
    await page.getByRole('button', { name: 'Connect browser', exact: true }).click();
    await expect(page.getByText(/awman link web-[A-Z0-9]{4}-[A-Z0-9]{4}/)).toBeVisible();
  });
}
