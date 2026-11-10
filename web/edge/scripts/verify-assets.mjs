import { createHash } from 'node:crypto';
import { mkdir, readFile, readdir, stat, writeFile } from 'node:fs/promises';
import { dirname, resolve, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { auditHtml, auditSvg, wasmPolicyProblems } from './asset-policy.mjs';

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const assetRoot = resolve(packageRoot, '../out');
const entries = [];
const failures = [];
async function walk(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const file = resolve(directory, entry.name);
    if (entry.isSymbolicLink()) throw new Error('Symlink in static export');
    if (entry.isDirectory()) { await walk(file); continue; }
    const name = relative(assetRoot, file).split(sep).join('/');
    if (!entry.isFile()) throw new Error('Non-file in static export');
    const body = await readFile(file);
    entries.push({ path: name, sha256: createHash('sha256').update(body).digest('hex'), bytes: body.length });
    if (name.endsWith('.html')) {
      const audit = auditHtml(body.toString('utf8'));
      for (const problem of audit.problems) failures.push(`${name}: ${problem}`);
      for (const reference of audit.localReferences) {
        const target = new URL(reference, `https://static.invalid/${name}`);
        let candidate;
        try { candidate = resolve(assetRoot, '.' + decodeURIComponent(target.pathname)); }
        catch { failures.push(`${name}: malformed resource path`); continue; }
        if (!candidate.startsWith(assetRoot + sep) || !(await stat(candidate).catch(() => null))?.isFile()) failures.push(`${name}: missing local resource`);
      }
    }
    if (name.endsWith('.map')) failures.push(`${name}: public source map`);
    if (/\.(?:svg|xml|xhtml|xht)$/i.test(name)) for (const problem of auditSvg(body.toString('utf8'))) failures.push(`${name}: ${problem}`);
  }
}
try {
  await walk(assetRoot);
  if (!entries.some(entry => entry.path === 'index.html')) failures.push('Missing index.html');
  // The CSP lives in TypeScript (src/security.ts); this plain-Node gate reads
  // its single switch rather than compiling the module.
  const security = (await readFile(resolve(packageRoot, 'src/security.ts'), 'utf8')).replace(/^\s*\/\/.*$/gm, '');
  const wasmFlag = /TERMINAL_REQUIRES_WASM_UNSAFE_EVAL = (true|false);/.exec(security)?.[1];
  if (!wasmFlag) failures.push('src/security.ts: cannot read TERMINAL_REQUIRES_WASM_UNSAFE_EVAL');
  const scriptSrc = `script-src 'self'${wasmFlag === 'true' ? " 'wasm-unsafe-eval'" : ''}${/'unsafe-eval'/.test(security) ? " 'unsafe-eval'" : ''}`;
  for (const problem of wasmPolicyProblems(entries.map(entry => entry.path), scriptSrc)) failures.push(problem);
  if (failures.length) throw new Error(failures.join('\n'));
  entries.sort((a, b) => a.path.localeCompare(b.path));
  await mkdir(resolve(packageRoot, '.artifacts'), { recursive: true });
  await writeFile(resolve(packageRoot, '.artifacts/asset-integrity.json'), JSON.stringify({ algorithm: 'sha256', assets: entries }, null, 2) + '\n');
  process.stdout.write(`Verified ${entries.length} static assets; SHA-256 inventory: web/edge/.artifacts/asset-integrity.json\n`);
} catch (error) {
  process.stderr.write(`Static export verification failed: ${error instanceof Error ? error.message : 'invalid export'}\n`);
  process.exitCode = 1;
}
