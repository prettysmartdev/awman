#!/usr/bin/env node
// Recreate each web package's dependencies for the current host architecture.
import { spawnSync } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

// Webpack caches loader resolution under .next. Replacing node_modules alone
// leaves that state behind, including paths from another host or installation.
console.log('Clearing the Next.js build cache (stop the dev server before reinstalling)');
rmSync(fileURLToPath(new URL('../web/.next', import.meta.url)), { recursive: true, force: true });

for (const packageDir of ['web', 'web/edge', 'web/e2e']) {
  const cwd = fileURLToPath(new URL(`../${packageDir}/`, import.meta.url));
  const dependencies = fileURLToPath(new URL(`../${packageDir}/node_modules`, import.meta.url));
  if (!existsSync(fileURLToPath(new URL(`../${packageDir}/package-lock.json`, import.meta.url)))) {
    throw new Error(`${packageDir}/package-lock.json is missing`);
  }
  console.log(`Reinstalling ${packageDir} dependencies for this machine`);
  rmSync(dependencies, { recursive: true, force: true });
  const result = spawnSync('npm', ['ci', '--no-audit', '--no-fund'], { cwd, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
