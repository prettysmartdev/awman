import { expect, type Page } from '@playwright/test';
import { test, renderedGrid } from './terminal-geometry-helper.js';

type Resize = { type: 'resize'; cols: number; rows: number };
type Sent = { url: string; frame: Resize; at: number };
function observeResizes(page: Page) {
  const sent: Sent[] = [];
  page.on('websocket', socket => socket.on('framesent', event => {
    const frame = JSON.parse(String(event.payload));
    if (frame.type === 'resize') sent.push({ url: socket.url(), frame, at: Date.now() });
  }));
  return sent;
}
const terminal = (page: Page, index = 0) => page.getByRole('region', { name: `Terminal: Agent ${index}`, exact: true });
async function ready(page: Page) {
  await expect(terminal(page)).toContainText('Viewing');
  await expect(terminal(page).getByRole('log')).toContainText('PTY_SIZE');
}
async function resized(page: Page, sent: Sent[], index = 0) {
  const size = await renderedGrid(page, index);
  await expect.poll(() => sent.at(-1)?.frame).toEqual({ type: 'resize', ...size });
  // This string is emitted by the real PTY's SIGWINCH handler running stty,
  // then transported and parsed by the production renderer (no local echo).
  await expect(terminal(page, index).getByRole('log')).toContainText(`PTY_SIZE ${size.rows} ${size.cols}`);
  return size;
}

test('holder geometry reaches the daemon PTY and redraws across window, panel, font and visibility changes', async ({ page }) => {
  const sent = observeResizes(page);
  const violations: string[] = [];
  page.on('console', msg => { if (msg.text().includes('Content Security Policy')) violations.push(msg.text()); });
  await page.goto('/');
  await ready(page);
  expect(sent).toEqual([]);
  await terminal(page).getByRole('button', { name: 'Take control', exact: true }).click();
  await resized(page, sent);
  await page.waitForTimeout(200);

  for (const size of [{ width: 1280, height: 720 }, { width: 1440, height: 900 }, { width: 1024, height: 768 }, { width: 768, height: 1024 }, { width: 390, height: 844 }]) {
    const before = sent.length;
    const start = Date.now();
    await page.setViewportSize(size);
    await resized(page, sent);
    await page.waitForTimeout(200);
    expect(sent.slice(before)).toHaveLength(1);
    expect(sent.at(-1)!.at - start).toBeGreaterThanOrEqual(120);
  }
  await page.setViewportSize({ width: 1280, height: 720 });
  await resized(page, sent);
  await page.waitForTimeout(200);

  // A layout storm without a window resize coalesces to the final viewport.
  const before = sent.length;
  await page.evaluate(() => {
    const root = document.querySelector('#root')!;
    root.classList.add('inspector'); root.classList.remove('inspector'); root.classList.add('inspector');
  });
  await resized(page, sent);
  await page.waitForTimeout(200);
  expect(sent.slice(before)).toHaveLength(1);

  // The probe changes while the container stays fixed.
  const fontBefore = await renderedGrid(page);
  await page.getByRole('button', { name: 'font', exact: true }).click();
  const fontAfter = await resized(page, sent);
  expect(fontAfter.cols).toBeLessThan(fontBefore.cols);
  expect(fontAfter.rows).toBeLessThan(fontBefore.rows);
  await page.waitForTimeout(200);

  await page.getByRole('button', { name: 'zoom', exact: true }).click();
  const zoomed = await resized(page, sent);
  expect(zoomed.cols).toBeLessThan(fontAfter.cols);
  await page.waitForTimeout(200);
  await page.getByRole('button', { name: 'zoom', exact: true }).click();
  await resized(page, sent);
  await page.waitForTimeout(200);

  for (const visibility of ['hide', 'invisible']) {
    await page.getByRole('button', { name: visibility, exact: true }).click();
    const hiddenCount = sent.length;
    await page.setViewportSize({ width: 1120, height: 760 });
    await page.getByRole('button', { name: 'font', exact: true }).click();
    await page.waitForTimeout(350);
    expect(sent).toHaveLength(hiddenCount);
    await page.getByRole('button', { name: visibility, exact: true }).click();
    await resized(page, sent);
    await page.waitForTimeout(200);
  }
  expect(sent.every(({ frame }) => Number.isFinite(frame.cols) && Number.isFinite(frame.rows) && frame.cols > 0 && frame.rows > 0)).toBe(true);
  expect(violations).toEqual([]);
});

test('read-only viewer emits no resize and parallel terminals retain independent geometry', async ({ page, context }) => {
  const sent = observeResizes(page);
  await page.goto('/'); await ready(page);
  await terminal(page).getByRole('button', { name: 'Take control', exact: true }).click();
  await resized(page, sent);

  const viewer = await context.newPage();
  const viewerFrames = observeResizes(viewer);
  await viewer.goto('/?viewer');
  await expect(terminal(viewer)).toContainText('Read-only');
  await viewer.setViewportSize({ width: 768, height: 1024 });
  for (const name of ['layout', 'font', 'hide', 'hide']) await viewer.getByRole('button', { name, exact: true }).click();
  await viewer.waitForTimeout(350);
  expect(viewerFrames).toEqual([]);
  await expect(terminal(viewer).getByRole('button', { name: 'Take control', exact: true })).toHaveCount(0);
  await viewer.close();

  await page.getByRole('button', { name: 'parallel', exact: true }).click();
  await resized(page, sent);
  await terminal(page, 1).getByRole('button', { name: 'Take control', exact: true }).click();
  const second = await resized(page, sent, 1);
  const first = await renderedGrid(page);
  expect(second.cols).not.toBe(first.cols);
  expect(new Set(sent.map(frame => frame.url)).size).toBe(2);
  await expect(terminal(page).getByRole('log')).toContainText(`PTY_SIZE ${first.rows} ${first.cols}`);
});

test('reconnect drops control, then recomputes current geometry when control is granted again', async ({ page }) => {
  const sent = observeResizes(page);
  await page.goto('/'); await ready(page);
  await terminal(page).getByRole('button', { name: 'Take control', exact: true }).click();
  await resized(page, sent);
  await page.waitForTimeout(200);
  const before = sent.length;
  await page.getByRole('button', { name: 'reconnect', exact: true }).click();
  await page.setViewportSize({ width: 1050, height: 750 });
  await page.getByRole('button', { name: 'layout', exact: true }).click();
  await expect(terminal(page).getByRole('button', { name: 'Take control', exact: true })).toBeVisible();
  await page.waitForTimeout(350);
  expect(sent).toHaveLength(before);
  await terminal(page).getByRole('button', { name: 'Take control', exact: true }).click();
  await resized(page, sent);
  expect(sent).toHaveLength(before + 1);
});
