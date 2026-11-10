import { expect, it } from 'vitest';
import { auditHtml, auditSvg, configKeyProblems, wasmPolicyProblems } from '../scripts/asset-policy.mjs';

it('accepts self-hosted external scripts/styles and records their build references', () => {
  expect(auditHtml('<link rel="stylesheet" href="/app.css"><script src="/_next/a.js" defer></script>'))
    .toEqual({ problems: [], localReferences: ['/app.css', '/_next/a.js'] });
});
it.each([
  ['<script>alert(1)</script>', 'inline-script'],
  ['<script src="/ok.js">alert(1)</script>', 'inline-script'],
  ['<style>body{color:red}</style>', 'inline-style'],
  ['<div style="color:red">x</div>', 'inline-style'],
  ['<img src="/x.png" onerror="alert(1)">', 'inline-handler'],
  ['<script src="https://third-party.invalid/x.js"></script>', 'nonlocal-resource'],
  ['<link rel="stylesheet" href="//third-party.invalid/x.css">', 'nonlocal-resource'],
  ['<iframe src="/embedded"></iframe>', 'embedded-or-base-content'],
  ['<base href="https://third-party.invalid/">', 'embedded-or-base-content'],
  ['<meta http-equiv="refresh" content="0;url=https://third-party.invalid/">', 'http-equiv'],
])('blocks incompatible static export %s', (html, problem) => {
  expect(auditHtml(html).problems).toContain(problem);
});

it('requires wasm-unsafe-eval (never unsafe-eval) when the export ships WebAssembly', async () => {
  const { CONTENT_SECURITY_POLICY } = await import('../src/security');
  const wasm = ['index.html', '_next/static/media/ghostty-vt.0000.wasm'];
  expect(wasmPolicyProblems(wasm, CONTENT_SECURITY_POLICY)).toEqual([]);
  expect(wasmPolicyProblems(wasm, "default-src 'none'; script-src 'self'")).toHaveLength(1);
  expect(wasmPolicyProblems(['index.html'], "script-src 'self'")).toEqual([]);
  expect(wasmPolicyProblems(['index.html'], "script-src 'self' 'unsafe-eval'")).toHaveLength(1);
});

it('rejects deployment config keys outside the allowlist (privacy review #4)', () => {
  const base = { name: 'awman-web', main: 'src/index.ts', observability: { enabled: false } };
  expect(configKeyProblems(base)).toEqual([]);
  expect(configKeyProblems({ ...base, tail_consumers: [{ service: 'collector' }] })).toEqual(['wrangler.jsonc: unexpected top-level key "tail_consumers"']);
  expect(configKeyProblems({ ...base, env: { production: { observability: { enabled: true } } } })).toHaveLength(1);
  for (const key of ['analytics_engine_datasets', 'services', 'queues', 'd1_databases', 'kv_namespaces', 'r2_buckets', 'vars']) {
    expect(configKeyProblems({ ...base, [key]: [] })).toHaveLength(1);
  }
});

it.each([
  ['<img srcset="/a.png 1x, https://third-party.invalid/t.png 2x">', 'nonlocal-resource'],
  ['<link rel="preconnect" href="https://third-party.invalid">', 'nonlocal-resource'],
  ['<link rel="dns-prefetch" href="//third-party.invalid">', 'nonlocal-resource'],
  ['<a href="javascript:alert(1)">x</a>', 'javascript-url'],
])('closes asset-gate gaps (privacy review #5): %s', (html, problem) => {
  expect(auditHtml(html).problems).toContain(problem);
});
it('keeps ordinary local srcset and outbound <a> links valid', () => {
  expect(auditHtml('<img srcset="/a.png 1x, /b.png 2x"><a href="https://example.invalid/docs">docs</a>').problems).toEqual([]);
});
it('rejects scriptable SVG documents', () => {
  expect(auditSvg('<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0"/></svg>')).toEqual([]);
  expect(auditSvg('<svg><script>alert(1)</script></svg>')).toContain('svg-script');
  expect(auditSvg('<svg onload="alert(1)"></svg>')).toContain('svg-handler');
});

it('sends a sandboxing CSP on SVG/XML documents and HSTS everywhere', async () => {
  const { secureResponse } = await import('../src/security');
  const svg = secureResponse(new Response('<svg/>', { headers: { 'Content-Type': 'image/svg+xml' } }));
  expect(svg.headers.get('Content-Security-Policy')).toBe("default-src 'none'; sandbox");
  expect(svg.headers.get('Strict-Transport-Security')).toContain('max-age=31536000');
  const js = secureResponse(new Response('x', { headers: { 'Content-Type': 'text/javascript' } }));
  expect(js.headers.has('Content-Security-Policy')).toBe(false);
});
