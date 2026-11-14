import { test as base, expect } from '@playwright/test';
import { createServer } from 'node:http';
import { readFile, readdir } from 'node:fs/promises';
import { extname, resolve, sep } from 'node:path';
import { Daemon, origin, root } from './daemon.js';
import { contentSecurityPolicy } from '../edge/src/security.js';

// Production-equivalent document policy: the exact edge CSP. The strict
// variant (no 'wasm-unsafe-eval') exists only as the negative control proving
// the WASM fixture really exercises CSP enforcement.
const CONTENT_SECURITY_POLICY = contentSecurityPolicy(origin);
export const STRICT_CSP_WITHOUT_WASM = CONTENT_SECURITY_POLICY.replace(" 'wasm-unsafe-eval'", '');
const WASM_FIXTURE_JS = `(async () => {
  try {
    const response = await fetch(document.documentElement.dataset.wasm);
    await WebAssembly.compile(await response.arrayBuffer());
    document.title = 'wasm-ok';
  } catch (error) { document.title = 'wasm-blocked'; }
})();`;

export const test = base.extend<{ daemon: Daemon }, { shell: void }>({
  shell: [async ({}, use) => {
    // AWMAN_E2E_WEB_OUT serves another export directory; used only for
    // documented negative controls (a deliberately mutated copy outside the repo).
    const output = process.env.AWMAN_E2E_WEB_OUT ? resolve(process.env.AWMAN_E2E_WEB_OUT) : resolve(root, 'web/out');
    const server = createServer(async (req, res) => {
      res.setHeader('Cache-Control', 'no-store');
      const url = new URL(req.url!, 'http://localhost');
      if (url.pathname === '/__csp_wasm_fixture.js') {
        res.setHeader('Content-Type', 'application/javascript');
        res.end(WASM_FIXTURE_JS);
        return;
      }
      if (url.pathname === '/__csp_wasm_fixture') {
        // Compiles the shipped terminal core (the exact hashed .wasm in web/out)
        // under the production CSP, as the terminal does on open.
        const media = (await readdir(resolve(output, '_next/static/media'))).find(name => name.endsWith('.wasm'));
        res.setHeader('Content-Type', 'text/html');
        res.setHeader('Content-Security-Policy', url.searchParams.has('strict') ? STRICT_CSP_WITHOUT_WASM : CONTENT_SECURITY_POLICY);
        res.end(`<!doctype html><html data-wasm="/_next/static/media/${media}"><title>pending</title><script src="/__csp_wasm_fixture.js"></script></html>`);
        return;
      }
      // Explicit transport-only harness. Never reported as an application test.
      if (req.url === '/__transport_fixture') {
        res.setHeader('Content-Type', 'text/html');
        res.end('<!doctype html><title>Listener transport fixture</title><button>Probe</button>');
        return;
      }
      const path = resolve(output, '.' + new URL(req.url!, 'http://localhost').pathname.replace(/\/$/, '/index.html'));
      if (!path.startsWith(output + sep)) { res.writeHead(403).end(); return; }
      try {
        const bytes = await readFile(path);
        res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'application/javascript', '.css': 'text/css', '.json': 'application/json', '.wasm': 'application/wasm' } as Record<string, string>)[extname(path)] ?? 'application/octet-stream');
        if (extname(path) === '.html') res.setHeader('Content-Security-Policy', CONTENT_SECURITY_POLICY);
        res.end(bytes);
      } catch { res.writeHead(503).end('Required WI 0159 built web shell is missing'); }
    });
    await new Promise<void>((ok, fail) => {
      server.once('error', fail);
      server.listen(3000, '127.0.0.1', ok);
    });
    try { await use(); } finally {
      server.closeAllConnections();
      await new Promise<void>((ok, fail) => server.close(e => e ? fail(e) : ok()));
    }
  }, { scope: 'worker', auto: true }],
  daemon: async ({}, use) => {
    const daemon = new Daemon();
    try { await daemon.init(); await use(daemon); } finally { await daemon.dispose(); }
  },
});
export { expect };
