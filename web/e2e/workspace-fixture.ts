// Real-component layout fixture. Bundles the production Workspace (with
// Panels, TerminalPanel, TerminalView, Composer, Dialogs, ShellLayout) and the
// production stylesheets with esbuild (pinned in web/package.json), and serves
// it under the exact production CSP with the shipped terminal WASM core.
//
// Only two things are fakes, both at the network boundary: the engine
// connection is an in-memory InstanceStore fed from the recorded engine
// snapshot fixture, and TerminalConnection is replaced by a recording fake
// that debounces resize exactly like the real one (150 ms, cancel on refit)
// and echoes the new size back. No daemon, PTY or credential is involved.
import { createServer, type Server } from 'node:http';
import { createRequire } from 'node:module';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { extname, join, resolve } from 'node:path';
import { root } from './daemon.js';
import { contentSecurityPolicy } from '../edge/src/security.js';

const require = createRequire(resolve(root, 'web/package.json'));

const entry = String.raw`
import React from 'react';
import { createRoot } from 'react-dom/client';
import { Workspace } from '@/components/Workspace';
import { InstanceStore } from '@/lib/client/instanceStore';
import { Subject } from '@/lib/client/observable';
import frames from '../test/lib/fixtures/engine-frames.json';
const q = new URLSearchParams(location.search);
const source = structuredClone(frames.find(f => f.type === 'snapshot'));
source.pending_dialogs = [];
const base = source.tabs[0];
source.tabs = Array.from({ length: Number(q.get('tabs') || 1) }, (_, i) => {
  const tab = structuredClone(base); tab.tab_id = 'tab_' + i;
  tab.label.text = i ? 'Fixture tab ' + i + ' with a fairly long label' : 'Build the application shell';
  return tab;
});
source.active_tab_hint = 'tab_0';
for (const tab of source.tabs) {
  tab.session.cwd = '/workspace/projects/awman/feature/layout-fixture'; tab.yolo = null;
  const first = tab.slots[0];
  tab.slots = Array.from({ length: Number(q.get('slots') || 1) }, (_, i) => {
    const slot = structuredClone(first); slot.slot_id = tab.tab_id + '_slot_' + i; slot.yolo = null; slot.acp = null;
    slot.terminal_id = tab.tab_id + '_term_' + i; slot.step_name = i ? 'review-' + i : 'implement';
    return slot;
  });
  tab.focused_slot = tab.slots[0]?.slot_id ?? null;
  tab.execution_phase = q.get('phase') === 'running' ? { phase: 'running', command: 'exec workflow feature.toml' } : { phase: 'idle' };
  if (q.get('countdown') === '1') tab.yolo = { step_name: 'implement', remaining_secs: 30, kind: 'stuck_step' };
}
const store = new InstanceStore(); store.applySnapshot(source);
const names = ['ready', 'chat', 'attach', 'exec', 'specs', 'config', 'status', 'squad', 'queue', 'daemon', 'remote', 'new'];
const client = {
  catalogue: async () => ({ etag: 'fixture', actions: { show_config: ['config', 'show'] }, commands: names.map(name => ({ path: [name], help: 'Run ' + name + ' with the usual options', subcommands: [], flags: [], arguments: [], aliases: [], long_help: null })) }),
  prompt: async () => ({ title: 'Confirm', body: 'Fixture question', choices: [], default_on_dismiss: 'dismiss' }),
  launchContexts: async () => ({ items: [], default_id: null }),
};
const sent = [];
const connection = {
  store, state: new Subject('open'), role: new Subject(q.get('role') || 'primary'), id: 'fixture', close() {}, send(frame) { sent.push(frame); },
  uiRequest: async request => { sent.push({ ui: request }); return {}; }, onFrame: () => () => {},
  queueRequest: async () => ({ result: 'snapshot', value: { items: [], loaded: true, new_since_seen: 0, for_you: 0 } }),
};
window.__fixture = { store, connection, sent, resizes: [], terminals: [] };
createRoot(document.getElementById('root')).render(<Workspace client={client} connection={connection} launchContext={null} squadAvailable={false} reconnect={() => {}} />);
`;

const fakeTerminal = String.raw`
import { Subject } from '@/lib/client/observable';
const text = new TextEncoder();
export class TerminalConnection {
  constructor(client, terminalId, opts = {}) {
    this.terminalId = terminalId; this.readOnly = !!opts.readOnly; this.closed = false;
    this.state = new Subject('open');
    const viewer = new URLSearchParams(location.search).get('viewer') === '1';
    this.control = new Subject({ holder: this.readOnly || viewer ? 'none' : 'me', inputAllowed: !this.readOnly });
    this.listeners = { snapshot: new Set(), resize: new Set(), output: new Set(), exit: new Set(), error: new Set() };
    this.timer = null;
    window.__fixture.terminals.push(this);
    setTimeout(() => { for (const cb of this.listeners.snapshot) cb({ cols: 80, rows: 24, hasOutput: true, screen: text.encode('fixture-agent$ ready\r\n') }); }, 0);
  }
  on(kind, cb) { this.listeners[kind].add(cb); return () => this.listeners[kind].delete(cb); }
  onSnapshot(cb) { return this.on('snapshot', cb); } onOutput(cb) { return this.on('output', cb); }
  onResize(cb) { return this.on('resize', cb); } onExit(cb) { return this.on('exit', cb); } onError(cb) { return this.on('error', cb); }
  resize(cols, rows) {
    const control = this.control.get();
    if (control.holder !== 'me' || !control.inputAllowed || this.closed) return;
    clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      window.__fixture.resizes.push({ terminal: this.terminalId, cols, rows, at: performance.now() });
      for (const cb of this.listeners.resize) cb(cols, rows);
    }, 150);
  }
  cancelResize() { clearTimeout(this.timer); this.timer = null; }
  input(bytes) { (window.__fixture.inputs ??= []).push([this.terminalId, ...bytes]); }
  async takeControl() { this.control.set({ holder: 'me', inputAllowed: true }); (window.__fixture.takes ??= []).push(this.terminalId); }
  close() { this.closed = true; this.cancelResize(); (window.__fixture.closed ??= []).push(this.terminalId); }
}
`;

const types: Record<string, string> = { '.html': 'text/html', '.js': 'application/javascript', '.css': 'text/css', '.wasm': 'application/wasm' };

export class WorkspaceFixture {
  origin = '';
  private directory = '';
  private server?: Server;
  async start() {
    this.directory = await mkdtemp(join(tmpdir(), 'awman-workspace-fixture-'));
    const { build } = require('esbuild') as typeof import('esbuild');
    await build({
      stdin: { contents: entry, resolveDir: resolve(root, 'web/src'), loader: 'tsx', sourcefile: 'workspace-fixture.tsx' },
      bundle: true, format: 'esm', outfile: join(this.directory, 'fixture.js'), tsconfig: resolve(root, 'web/tsconfig.json'),
      jsx: 'automatic', loader: { '.module.css': 'local-css', '.css': 'css' }, define: { 'process.env.NODE_ENV': '"production"' }, logLevel: 'silent',
      plugins: [{ name: 'fake-terminal-connection', setup(b) {
        b.onResolve({ filter: /lib\/client\/terminalSocket$/ }, () => ({ path: 'fake-terminal', namespace: 'fixture' }));
        b.onLoad({ filter: /.*/, namespace: 'fixture' }, () => ({ loader: 'js', resolveDir: resolve(root, 'web/src'), contents: fakeTerminal }));
      } }],
    });
    // The production global stylesheet, bundled with its @imports.
    await build({ entryPoints: [resolve(root, 'web/src/app/globals.css')], bundle: true, outfile: join(this.directory, 'globals.css'), logLevel: 'silent' });
    const wasm = require.resolve('@wterm/ghostty/ghostty-vt.wasm');
    const files: Record<string, string> = { '/fixture.js': join(this.directory, 'fixture.js'), '/fixture.css': join(this.directory, 'fixture.css'), '/globals.css': join(this.directory, 'globals.css'), '/wasm/ghostty-vt.wasm': wasm };
    this.server = createServer(async (req, res) => {
      res.setHeader('Cache-Control', 'no-store');
      const path = new URL(req.url ?? '/', 'http://localhost').pathname;
      if (path === '/') {
        res.setHeader('Content-Type', 'text/html');
        res.setHeader('Content-Security-Policy', contentSecurityPolicy(this.origin));
        res.end('<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Workspace layout fixture</title><link rel="stylesheet" href="/globals.css"><link rel="stylesheet" href="/fixture.css"></head><body><div id="root"></div><script type="module" src="/fixture.js"></script></body></html>');
        return;
      }
      const file = files[path];
      // A fixed allow-list: nothing else on disk is reachable.
      if (!file) { res.writeHead(404).end(); return; }
      try { res.setHeader('Content-Type', types[extname(file)] ?? 'application/octet-stream'); res.end(await readFile(file)); }
      catch { res.writeHead(404).end(); }
    });
    await new Promise<void>((ok, fail) => {
      this.server!.once('error', fail);
      this.server!.listen(0, '127.0.0.1', () => {
        const address = this.server!.address();
        if (!address || typeof address === 'string') { fail(new Error('workspace fixture did not bind a TCP port')); return; }
        this.origin = `http://127.0.0.1:${address.port}`; ok();
      });
    });
  }
  url(query = '') { return `${this.origin}/?${query}`; }
  async stop() {
    if (this.server) { this.server.closeAllConnections(); await new Promise<void>(ok => this.server!.close(() => ok())); }
    if (this.directory) await rm(this.directory, { recursive: true, force: true });
  }
}
