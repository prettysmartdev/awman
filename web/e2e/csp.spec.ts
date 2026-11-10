import { test, expect } from './fixtures.js';

// Production CSP acceptance (the exact edge policy is served by the shell
// fixture). Every agent terminal compiles the self-hosted Ghostty WASM core,
// so the policy must allow it; the strict variant is the negative control.
test('production CSP compiles the shipped terminal WASM core', async ({ page }) => {
  const violations: string[] = [];
  page.on('console', message => { if (/Content Security Policy/i.test(message.text())) violations.push(message.text()); });
  await page.goto('/__csp_wasm_fixture');
  await expect(page).toHaveTitle('wasm-ok');
  expect(violations).toEqual([]);
});

test('without wasm-unsafe-eval the same core is refused (negative control)', async ({ page }) => {
  await page.goto('/__csp_wasm_fixture?strict');
  await expect(page).toHaveTitle('wasm-blocked');
});

test('the app shell loads under the production CSP without violations', async ({ page }) => {
  const violations: string[] = [];
  page.on('console', message => { if (/Content Security Policy/i.test(message.text())) violations.push(message.text()); });
  await page.goto('/');
  await expect(page.locator('body')).not.toBeEmpty();
  await page.waitForLoadState('networkidle');
  expect(violations).toEqual([]);
});
