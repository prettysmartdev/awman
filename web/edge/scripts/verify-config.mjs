import { readFile } from 'node:fs/promises';
import { configKeyProblems } from './asset-policy.mjs';
import assert from 'node:assert/strict';
const root = new URL('../', import.meta.url);
const config = JSON.parse(await readFile(new URL('wrangler.jsonc', root), 'utf8'));
const pkg = JSON.parse(await readFile(new URL('package.json', root), 'utf8'));
assert.equal(config.assets.directory, '../out');
assert.equal(config.assets.binding, 'ASSETS');
assert.equal(config.assets.run_worker_first, true);
assert.equal(config.assets.not_found_handling, '404-page');
assert.equal(config.observability.enabled, false);
assert.equal(config.observability.logs.enabled, false);
assert.equal(config.observability.logs.invocation_logs, false);
assert.equal(config.observability.traces.enabled, false);
assert.equal(config.logpush, false);
assert.equal(config.send_metrics, false);
assert.equal(config.upload_source_maps, false);
assert.equal(config.workers_dev, false);
assert.equal(config.preview_urls, false);
assert.deepEqual(config.routes, [{ pattern: 'awman.prettysmart.dev', custom_domain: true }]);
assert.deepEqual(config.durable_objects.bindings, [{ name: 'USAGE', class_name: 'UsageCounter' }]);
assert.deepEqual(config.migrations, [{ tag: 'v1', new_sqlite_classes: ['UsageCounter'] }]);
assert.equal(config.vars, undefined, 'Secrets must be supplied as secret bindings');
for (const version of Object.values({ ...pkg.dependencies, ...pkg.devDependencies })) {
  assert.match(version, /^\d+\.\d+\.\d+(?:-[\w.-]+)?$/, 'Dependencies must have exact versions');
}
const keyProblems = configKeyProblems(config);
assert.deepEqual(keyProblems, [], keyProblems.join('\n'));
