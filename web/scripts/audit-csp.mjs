// CSP-compatibility audit of the static export (design §5.1, §5.8). The
// production policy (web/edge/src/security.ts) is `script-src 'self' 'wasm-unsafe-eval'`,
// `style-src 'self'`, no 'unsafe-inline', no 'unsafe-eval'. This fails on:
//   - any inline <script> (no src, or a src plus a body), <style>, style=
//     attribute or on*= handler attribute in any HTML file;
//   - javascript: URLs, <iframe>/<object>/<embed>/<frame>/<base>, and
//     <meta http-equiv> (headers belong to the edge);
//   - eval( / new Function( / string-argument setTimeout in shipped JS;
// and lists every .wasm asset (the terminal core; the edge CSP grants
// 'wasm-unsafe-eval' for it and edge verify:assets enforces that).
import { readFile, readdir } from 'node:fs/promises';
import { join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parse } from 'parse5';

const EVAL_RES = [
  [/(?<![.\w$])eval\s*\(/, 'eval('],
  [/\bnew\s+Function\s*\(/, 'new Function('],
  [/(?<![\w$])Function\s*\(\s*["'`]/, 'Function("…")'],
  [/\bset(?:Timeout|Interval)\s*\(\s*["'`]/, 'string-argument timer'],
];

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else if (entry.isFile()) yield path;
  }
}

export function auditHtmlForCsp(html) {
  const problems = new Set();
  const visit = node => {
    const name = node.tagName;
    const attrs = new Map((node.attrs ?? []).map(attr => [attr.name.toLowerCase(), attr.value]));
    if (name === 'script') {
      const body = (node.childNodes ?? []).map(child => child.value ?? '').join('');
      if (!attrs.has('src') || body.trim()) problems.add('inline <script>');
    }
    if (name === 'style') problems.add('inline <style>');
    if (attrs.has('style')) problems.add('style= attribute');
    for (const attr of attrs.keys()) if (attr.startsWith('on')) problems.add(`${attr}= handler`);
    for (const attr of ['href', 'src', 'action', 'formaction']) {
      if (/^\s*javascript:/i.test(attrs.get(attr) ?? '')) problems.add('javascript: URL');
    }
    if (['iframe', 'frame', 'object', 'embed', 'base'].includes(name)) problems.add(`<${name}>`);
    if (name === 'meta' && attrs.has('http-equiv')) problems.add('<meta http-equiv>');
    for (const child of node.childNodes ?? []) visit(child);
    if (node.content) visit(node.content);
  };
  visit(parse(html));
  return [...problems];
}

// webpack's runtime and core-js resolve the global object with
// `Function("return this")()` only when `globalThis` is missing (never in a
// supported browser); under the CSP it would throw inside their try/catch.
// That exact expression is tolerated; any other dynamic code is not.
const GLOBAL_THIS_FALLBACK = /Function\("return this"\)\(\)/g;

export function auditJsForCsp(source) {
  const scrubbed = source.replace(GLOBAL_THIS_FALLBACK, '');
  return EVAL_RES.filter(([re]) => re.test(scrubbed)).map(([, what]) => what);
}

/** Returns { failures, wasm } for an export directory. */
export async function auditCsp(outDir) {
  const failures = [];
  const wasm = [];
  for await (const path of walk(outDir)) {
    const file = relative(outDir, path).split(sep).join('/');
    if (file.endsWith('.wasm')) wasm.push(file);
    if (file.endsWith('.html')) {
      for (const problem of auditHtmlForCsp(await readFile(path, 'utf8'))) failures.push(`${file}: ${problem}`);
    } else if (/\.m?js$/.test(file)) {
      for (const problem of auditJsForCsp(await readFile(path, 'utf8'))) failures.push(`${file}: ${problem}`);
    }
  }
  return { failures, wasm };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const outDir = resolve(process.argv[2] ?? 'out');
  try {
    const { failures, wasm } = await auditCsp(outDir);
    if (failures.length) {
      process.stderr.write(`audit-csp: ${failures.length} problem(s)\n${failures.join('\n')}\n`);
      process.exitCode = 1;
    } else {
      process.stdout.write(`audit-csp: no inline script/style/handlers or eval; wasm assets: ${wasm.length ? wasm.join(', ') : 'none'}\n`);
    }
  } catch (error) {
    process.stderr.write(`audit-csp failed: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
