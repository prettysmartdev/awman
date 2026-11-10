import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: '.',
  testMatch: '*.spec.ts',
  timeout: 60_000,
  expect: { timeout: 10_000 },
  workers: 1, // The hosted v0 endpoint is fixed; each test owns port 9877.
  fullyParallel: false,
  retries: 0,
  reporter: 'list',
  use: {
    baseURL: 'http://localhost:3000',
    // Pairing secrets must never be written into traces, videos or screenshots.
    trace: 'off', video: 'off', screenshot: 'off',
    serviceWorkers: 'block',
  },
  projects: ['chromium', 'firefox'].flatMap(browserName => [
    { name: `${browserName}-desktop`, use: { browserName: browserName as 'chromium' | 'firefox', viewport: { width: 1280, height: 800 } } },
    { name: `${browserName}-narrow`, use: { browserName: browserName as 'chromium' | 'firefox', viewport: { width: 390, height: 844 }, hasTouch: true } },
  ]),
});
