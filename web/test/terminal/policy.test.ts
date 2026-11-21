// Static guarantees for the terminal lane (design §5.5, WI 0159 §3): wterm is
// used only as a parser core — never its DOM/React renderer, WebSocket
// transport, shell, Markdown or WebMCP — and terminal code has no raw HTML,
// navigation, downloads or automatic clipboard paths.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { describe, expect, it } from 'vitest';

const require = createRequire(import.meta.url);
const WEB = dirname(require.resolve('../../package.json'));
const SRC = join(WEB, 'src/terminal');

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap(name => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return name === 'fixtures' ? [] : sources(path);
    return /\.(ts|tsx)$/.test(name) ? [path] : [];
  });
}

const files = sources(SRC).map(path => ({ path: path.slice(WEB.length + 1), text: readFileSync(path, 'utf8') }));
const code = (text: string) => text.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

describe('terminal lane policy', () => {
  it('imports only the headless wterm core', () => {
    for (const { path, text } of files) {
      const imports = [...code(text).matchAll(/(?:from\s+|import\s*\(\s*)['"]([^'"]+)['"]/g)].map(m => m[1]!);
      for (const spec of imports.filter(s => s.startsWith('@wterm/'))) expect(['@wterm/core', '@wterm/ghostty'], `${path} imports ${spec}`).toContain(spec);
    }
    const all = files.map(f => code(f.text)).join('\n');
    expect(all).not.toMatch(/WebSocketTransport|webmcp|WebMCP|modelContext/);
  });

  it('never opens sockets, writes HTML, navigates or downloads', () => {
    for (const { path, text } of files) {
      const body = code(text);
      expect(body, path).not.toMatch(/new\s+WebSocket|innerHTML|outerHTML|insertAdjacentHTML|document\.write|dangerouslySetInnerHTML/);
      expect(body, path).not.toMatch(/window\.open|location\.(href|assign|replace)|location\s*=|createObjectURL|\.download\s*=/);
      expect(body, path).not.toMatch(/\beval\s*\(|new\s+Function\b/);
    }
  });

  it('writes the clipboard only from copySelection (a user gesture)', () => {
    const writers = files.filter(f => /clipboard\??\.write/.test(code(f.text)));
    expect(writers.map(f => f.path)).toEqual(['src/terminal/TerminalView.tsx']);
    const view = code(writers[0]!.text);
    expect(view.match(/clipboard\??\.write/g)).toHaveLength(1);
    expect(view.slice(view.indexOf('const copySelection'), view.indexOf('const pasteFromClipboard'))).toMatch(/clipboard\??\.writeText/);
  });

  it('only the link list renders anchors, with noopener noreferrer', () => {
    const anchors = files.filter(f => /<a\s/.test(code(f.text)));
    expect(anchors.map(f => f.path)).toEqual(['src/terminal/links/LinkList.tsx']);
    expect(anchors[0]!.text).toContain('rel="noopener noreferrer"');
    expect(anchors[0]!.text).toContain('vetLink(');
  });

  it('pins the wterm packages exactly and does not depend on the DOM/React renderer', () => {
    const pkg = JSON.parse(readFileSync(join(WEB, 'package.json'), 'utf8')) as { dependencies: Record<string, string> };
    expect(pkg.dependencies['@wterm/core']).toBe('0.5.10');
    expect(pkg.dependencies['@wterm/ghostty']).toBe('0.5.10');
    expect(pkg.dependencies['@wterm/dom']).toBeUndefined();
    expect(pkg.dependencies['@wterm/react']).toBeUndefined();
  });
});
