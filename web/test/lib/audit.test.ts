import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
// @ts-expect-error -- plain .mjs build scripts without type declarations
import { auditOutput } from '../../scripts/audit-output.mjs';
// @ts-expect-error -- plain .mjs build scripts without type declarations
import { auditCsp, auditHtmlForCsp, auditJsForCsp } from '../../scripts/audit-csp.mjs';
// @ts-expect-error -- plain .mjs build scripts without type declarations
import { externalize } from '../../scripts/externalize-inline.mjs';
// @ts-expect-error -- plain .mjs build scripts without type declarations
import { isShellFile, precacheManifest, urlFor } from '../../scripts/build-sw.mjs';

let out: string;

async function put(path: string, body: string) {
  await mkdir(dirname(join(out, path)), { recursive: true });
  await writeFile(join(out, path), body);
}

const CLEAN_HTML =
  '<!DOCTYPE html><html><head><link rel="stylesheet" href="/_next/static/css/a.css"/><script src="/_next/static/chunks/a.js" async=""></script></head><body><main>awman</main></body></html>';

beforeEach(async () => {
  out = await mkdtemp(join(tmpdir(), 'awman-out-'));
  await put('index.html', CLEAN_HTML);
  await put('_next/static/chunks/a.js', 'new URL(p,"http://n")');
  await put('_next/static/css/a.css', 'body{margin:0}');
});

afterEach(async () => {
  await rm(out, { recursive: true, force: true });
});

describe('audit-output', () => {
  it('passes a clean export without a baked daemon address', async () => {
    expect(await auditOutput(out)).toEqual([]);
  });

  it('fails on a stray third-party origin', async () => {
    await put('_next/static/chunks/b.js', 'fetch("https://cdn.example/x.js")');
    const failures: string[] = await auditOutput(out);
    expect(failures.join('\n')).toMatch(/cdn\.example/);
  });

  it('fails on a non-loopback websocket origin', async () => {
    await put('_next/static/chunks/b.js', 'new WebSocket("wss://relay.example/ws")');
    expect((await auditOutput(out)).join('\n')).toMatch(/relay\.example/);
  });

  it('admits exactly the fixed loopback daemon destinations in shipped JavaScript (SR-01)', async () => {
    await put('_next/static/chunks/b.js', 'const H="http://127.0.0.1:9877",W="ws://127.0.0.1:9877";fetch(H+"/v1/health")');
    expect(await auditOutput(out)).toEqual([]);
  });

  it.each([
    'https://127.0.0.1:9877', 'wss://127.0.0.1:9877', 'http://127.0.0.1:9878', 'ws://localhost:9877',
    'https://awman.prettysmart.dev:9877', 'wss://awman.prettysmart.dev:9877', 'http://127.0.0.1:9877@evil.example',
  ])('fails on a moved or remote-protocol daemon destination %s', async url => {
    await put('_next/static/chunks/b.js', `fetch("${url}/v1/health")`);
    expect((await auditOutput(out)).join('\n')).toMatch(/disallowed network origin/);
  });

  it('fails on the daemon origin outside shipped JavaScript', async () => {
    await put('index.html', CLEAN_HTML.replace('awman', 'http://127.0.0.1:9877/v1/status'));
    expect((await auditOutput(out)).join('\n')).toMatch(/index\.html: disallowed network origin/);
  });

  it('fails on persistent storage APIs', async () => {
    await put('_next/static/chunks/b.js', 'window.localStorage.setItem("x","y")');
    expect((await auditOutput(out)).join('\n')).toMatch(/persistent storage/);
  });

  it('admits only the credential-database open call, in one file (WI 0160 C4)', async () => {
    await put('_next/static/chunks/b.js', 'function o(){return globalThis.indexedDB?.open("awman-browser-auth",1)??null}');
    expect(await auditOutput(out)).toEqual([]);
    await put('_next/static/chunks/c.js', 'indexedDB.open("other",1)');
    expect((await auditOutput(out)).join('\n')).toMatch(/c\.js: uses browser persistent storage/);
    await put('_next/static/chunks/c.js', 'indexedDB.open("awman-browser-auth",1)');
    expect((await auditOutput(out)).join('\n')).toMatch(/opened from several files/);
    await put('_next/static/chunks/c.js', 'x');
    await put('_next/static/chunks/b.js', 'indexedDB.open("awman-browser-auth",1);indexedDB.deleteDatabase("x")');
    expect((await auditOutput(out)).join('\n')).toMatch(/b\.js: uses browser persistent storage/);
    await put('_next/static/chunks/b.js', 'indexedDB.open("awman-browser-auth",1);localStorage.x=1');
    expect((await auditOutput(out)).join('\n')).toMatch(/b\.js: uses browser persistent storage/);
  });

  it('fails on session data or credentials in HTML', async () => {
    await put('index.html', CLEAN_HTML.replace('awman', 'awb_' + 'A'.repeat(43)));
    expect((await auditOutput(out)).join('\n')).toMatch(/browser grant token/);
  });

  it('fails on shipped source maps', async () => {
    await put('_next/static/chunks/a.js.map', '{}');
    expect((await auditOutput(out)).join('\n')).toMatch(/source map/);
  });
});

describe('audit-csp', () => {
  it('flags inline scripts, styles and handlers', () => {
    expect(auditHtmlForCsp('<script>alert(1)</script>')).toContain('inline <script>');
    expect(auditHtmlForCsp('<style>a{}</style>')).toContain('inline <style>');
    expect(auditHtmlForCsp('<div style="color:red"></div>')).toContain('style= attribute');
    expect(auditHtmlForCsp('<button onclick="x()"></button>')).toContain('onclick= handler');
    expect(auditHtmlForCsp('<meta http-equiv="Content-Security-Policy" content="x">')).toContain('<meta http-equiv>');
    expect(auditHtmlForCsp(CLEAN_HTML)).toEqual([]);
  });

  it('flags eval and new Function but tolerates the guarded globalThis fallback', () => {
    expect(auditJsForCsp('eval("1")')).toContain('eval(');
    expect(auditJsForCsp('new Function("return 1")')).toContain('new Function(');
    expect(auditJsForCsp('setTimeout("x()", 1)')).toContain('string-argument timer');
    expect(auditJsForCsp('try{return this||Function("return this")()}catch(e){}')).toEqual([]);
    expect(auditJsForCsp('obj.eval(1); medieval(2)')).toEqual([]);
  });

  it('reports wasm assets for the CSP decision', async () => {
    await put('_next/static/media/x.wasm', 'wasm');
    const { failures, wasm } = await auditCsp(out);
    expect(failures).toEqual([]);
    expect(wasm).toEqual(['_next/static/media/x.wasm']);
  });
});

describe('externalize-inline', () => {
  it('moves inline scripts to external files in document order', async () => {
    await put(
      'index.html',
      '<html><body><script src="/a.js" async=""></script><script>self.__next_f=[1]</script><p>x</p><script>self.__next_f.push(2)</script></body></html>',
    );
    const result = await externalize(out);
    expect(result.moved).toBe(2);
    const html = await readFile(join(out, 'index.html'), 'utf8');
    expect(auditHtmlForCsp(html)).toEqual([]);
    const sources = [...html.matchAll(/<script src="([^"]+)"/g)].map(m => m[1]);
    expect(sources[0]).toBe('/a.js');
    const bodies = await Promise.all(sources.slice(1).map(src => readFile(join(out, src!), 'utf8')));
    expect(bodies).toEqual(['self.__next_f=[1]', 'self.__next_f.push(2)']);
    expect((await readdir(join(out, '_next/static/inline'))).length).toBe(2);
  });

  it('refuses inline scripts it cannot externalize', async () => {
    await put('index.html', '<script type="application/json">{"a":1}</script>');
    await expect(externalize(out)).rejects.toThrow(/cannot be externalized/);
  });
});

describe('build-sw precache list', () => {
  it('includes only shell files', () => {
    expect(isShellFile('index.html')).toBe(true);
    expect(isShellFile('_next/static/chunks/a.js')).toBe(true);
    expect(isShellFile('icons/icon-192.png')).toBe(true);
    expect(isShellFile('manifest.webmanifest')).toBe(true);
    expect(isShellFile('index.txt')).toBe(false);
    expect(isShellFile('sw.js')).toBe(false);
    expect(isShellFile('_next/static/chunks/a.js.map')).toBe(false);
    expect(urlFor('index.html')).toBe('/');
  });

  it('derives a content version that changes with any shell byte', async () => {
    const first = await precacheManifest(out);
    expect(first.urls).toContain('/');
    await put('_next/static/css/a.css', 'body{margin:1px}');
    const second = await precacheManifest(out);
    expect(second.version).not.toBe(first.version);
  });
});
