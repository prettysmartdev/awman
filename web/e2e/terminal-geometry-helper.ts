// Geometry integration fixture. Builds the production TerminalView/connection
// and boots a real DaemonRuntime + API router with two actual portable PTYs.
// Only the agent program is a fixture; no WebSocket frames or resizes are mocked.
// Browser grants are seeded through the real grant store, then real origin-bound
// tickets authenticate every socket. Pairing UI is covered by link.spec.ts.
import { test as base, type Page } from '@playwright/test';
import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { mkdtemp, mkdir, readFile, writeFile, rm, copyFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { promisify } from 'node:util';
import { root, origin } from './daemon.js';
import { contentSecurityPolicy } from '../edge/src/security.js';

const execute = promisify(execFile);
const require = createRequire(resolve(root, 'web/package.json'));
const rust = String.raw`
use std::{io::{Read, Write}, sync::{Arc, Mutex}};
use awman::{command::{daemon_api::DaemonApi, daemon_runtime::{DaemonBootstrap, DaemonRuntime}},
    data::{browser_auth::{BrowserScope, BrowserGrantIssued}, config::env::EnvSnapshot,
        daemon_state::FeatureName, frontend_protocol::Hello, terminal::{TerminalOwner, TerminalOwnerInfo}},
    engine::terminal_hub::{TerminalHub, TerminalHubConfig, TerminalRegistration, TerminalInput, input_channel},
    frontend::api::routes::{build_router_for_listener, ListenerPolicy}};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::json;

#[tokio::main]
async fn main() {
    let home = std::env::var("HOME").unwrap();
    let repo = std::path::PathBuf::from(&home).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let env = EnvSnapshot::from_instance_env(std::env::vars().collect(), repo.clone(), vec!["awman".into()]);
    let config = awman::data::config::file::global_config_path(&env).unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, serde_json::to_vec(&json!({
        "runtime":{"kind":"docker"}, "agents":{"auth":{"refresh":{"enabled":false}}},
        "daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{"geometry":{
            "enabled":true,"listen":"127.0.0.1:9877","transport":"loopback_http",
            "features":["engine"],"allowedOrigins":["http://127.0.0.1:3000"]}}}}
    })).unwrap()).unwrap();
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
    let (runtime, claim) = DaemonRuntime::bootstrap(DaemonBootstrap {
        env: env.clone(), set: Default::default(),
        squad_run_frontends: awman::frontend::squad::unattended::UnattendedFrontends::shared(),
        push_frontend_factory: awman::frontend::api::ApiFrontendFactory,
        terminal_hub: hub.clone(),
        engine_presentation: Arc::new(awman::command::instance::frontend::PlainPresentation),
    }).await.unwrap();
    claim.finish();
    let api = Arc::new(DaemonApi::new(runtime.clone()));
    let host = runtime.engine_host();
    let context = host.launch_contexts().create(serde_json::from_value(json!({
        "root":repo,"label":"Geometry fixture","env":std::env::vars().collect::<std::collections::BTreeMap<_,_>>()
    })).unwrap()).unwrap();
    let instance = host.create_instance(Default::default()).unwrap();
    let (out, mut incoming) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move { while incoming.recv().await.is_some() {} });
    let hello: Hello = serde_json::from_value(json!({
        "protocol":"awman.frontend.v1","build":"geometry-fixture","kind":"web","role":"primary",
        "annotations":{},"launch_context":context.id,"term_size":{"cols":80,"rows":24},"view":null,"resume":null
    })).unwrap();
    let _frontend = host.attach_frontend(instance.clone(), hello, out).await.unwrap();
    let tab = host.tabs(&instance).unwrap()[0].tab_id.clone();
    let (record, token) = runtime.browser_auth().grants().create(
        "http://127.0.0.1:3000", "geometry", BrowserScope::Engine, Some("terminal geometry test"), chrono::Utc::now()
    ).unwrap();
    runtime.browser_auth().record_instance(&record.id, &instance);
    let grant = BrowserGrantIssued { id: record.id, token, scope: record.scope,
        origin: record.origin, listener: record.listener, expires_at: record.expires_at, reauth: None };
    let mut ids = Vec::new();
    let mut children = Vec::new();
    for index in 0..2 {
        let pair = native_pty_system().openpty(PtySize { cols:80, rows:24, pixel_width:0, pixel_height:0 }).unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "stty -echo; redraw() { printf '\\033[2J\\033[HPTY_SIZE '; stty size; }; trap redraw WINCH; redraw; while :; do read line || :; done"]);
        children.push(pair.slave.spawn_command(command).unwrap());
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let master = Arc::new(Mutex::new(pair.master));
        let resize_master = master.clone();
        let (stdin, mut inputs) = input_channel();
        let handle = hub.register(TerminalRegistration {
            owner: TerminalOwnerInfo { owner: TerminalOwner::Tab(tab.clone()), feature: FeatureName::Engine,
                source: None, attempt:1, allow_remote_input:false },
            agent:format!("geometry-{index}"), model:None, cols:80, rows:24,
        }, TerminalInput { stdin, resize:Some(Arc::new(move |cols, rows| {
            resize_master.lock().unwrap().resize(PtySize { cols, rows, pixel_width:0, pixel_height:0 }).unwrap();
        })) });
        ids.push(handle.id().clone());
        std::thread::spawn(move || {
            let _master = master;
            let mut bytes = [0;8192];
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 { break; }
                handle.push_output(&bytes[..n]);
            }
        });
        tokio::task::spawn_blocking(move || {
            while let Some(bytes) = inputs.blocking_recv() { if writer.write_all(&bytes).is_err() { break; } }
        });
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:9877").await.unwrap();
    runtime.report_listener_bound("geometry", listener.local_addr().unwrap());
    println!("{}", json!({"grant":grant,"ids":ids}));
    std::io::stdout().flush().unwrap();
    axum::serve(listener, build_router_for_listener(api, ListenerPolicy::Network { name:"geometry".into() })).await.unwrap();
    for mut child in children { let _ = child.kill(); }
}
`;

const entry = String.raw`
import React, { useSyncExternalStore } from 'react';
import { createRoot } from 'react-dom/client';
import { GhosttyCore } from '@wterm/ghostty';
import { createTerminalView } from '@/terminal/TerminalView';
import { TerminalConnection } from '@/lib/client/terminalSocket';
import { DaemonClient } from '@/lib/client/daemonClient';
const TerminalView = createTerminalView(async opts => {
  const core = await GhosttyCore.load({ wasmPath: '/ghostty.wasm', imageStorageLimit: 0 });
  core.init(opts.cols, opts.rows); return core;
});
const seed = await (await fetch('/seed.json')).json();
const client = new DaemonClient({ ...seed.grant, expiresAt: seed.grant.expires_at });
const connections = [];
const sockets = [];
window.geometry = { connections, sockets, ready: false };
function Panel({ id, index }) {
  const connection = connections[index];
  const control = useSyncExternalStore(cb => connection.control.subscribe(cb), () => connection.control.get());
  return <TerminalView connection={connection} title={'Agent ' + index} focused={false}
    control={control} onRequestControl={() => void connection.takeControl()} onLinkDetected={() => {}}
    scrollbackLines={1000} touch={false} reducedMotion theme="dark" />;
}
for (const [index, id] of seed.ids.entries()) {
  connections.push(new TerminalConnection(client, id, { readOnly: new URLSearchParams(location.search).has('viewer') }, {
    socketFactory(url, protocols) { const socket = new WebSocket(url, protocols); sockets.push(socket); return socket; }
  }));
}
createRoot(document.querySelector('#root')).render(<><div className="first"><Panel index={0} /></div><div className="second"><Panel index={1} /></div></>);
window.geometry.ready = true;
document.querySelector('#layout').onclick = () => document.querySelector('#root').classList.toggle('inspector');
document.querySelector('#hide').onclick = () => document.querySelector('#root').classList.toggle('hidden');
document.querySelector('#invisible').onclick = () => document.querySelector('#root').classList.toggle('invisible');
document.querySelector('#font').onclick = () => document.querySelector('#root').classList.toggle('large-font');
document.querySelector('#zoom').onclick = () => document.querySelector('#root').classList.toggle('zoomed');
document.querySelector('#parallel').onclick = () => document.querySelector('#root').classList.toggle('parallel');
document.querySelector('#reconnect').onclick = () => { for (const socket of sockets) if (socket.readyState === 1) socket.close(4000, 'geometry reconnect'); };
`;

const css = `html,body{margin:0;height:100%;font-family:system-ui}body{display:flex;flex-direction:column}nav{display:flex;flex-wrap:wrap;flex:none}button{min-height:32px}
#root{flex:1;min-height:0;display:flex}.first{flex:1;min-width:0;min-height:0}.second{display:none;min-width:0;min-height:0}.inspector{padding-right:180px}.hidden .first{display:none}.invisible .first{visibility:hidden}.large-font{--term-font-size:17px}.zoomed{zoom:1.25}.parallel .second{display:block;width:37%}`;

class GeometryServer {
  private directory = '';
  private child?: ChildProcess;
  private server?: ReturnType<typeof createServer>;
  async start() {
    this.directory = await mkdtemp(join(tmpdir(), 'awman-terminal-geometry-'));
    const source = join(this.directory, 'src');
    await mkdir(source);
    const manifest = `[package]\nname="awman-terminal-geometry-fixture"\nversion="0.0.0"\nedition="2021"\n[dependencies]\nawman={path=${JSON.stringify(root)}}\naxum={version="0.8",features=["ws"]}\ntokio={version="1",features=["full"]}\nportable-pty="0.9"\nserde_json="1"\nchrono="0.4"\n`;
    await writeFile(join(this.directory, 'Cargo.toml'), manifest);
    await copyFile(resolve(root, 'Cargo.lock'), join(this.directory, 'Cargo.lock'));
    await writeFile(join(source, 'main.rs'), rust);
    // Uses the repository target cache; never edits a workspace manifest/source.
    await execute('cargo', ['build', '--quiet', '--offline', '--manifest-path', join(this.directory, 'Cargo.toml'), '--target-dir', resolve(root, 'target')], { timeout: 240_000 });
    const home = join(this.directory, 'home');
    await mkdir(home);
    this.child = spawn(resolve(root, 'target/debug/awman-terminal-geometry-fixture'), [], {
      cwd: home, env: { PATH: process.env.PATH, HOME: home, AWMAN_HOME: join(home, 'data'), AWMAN_TEST_ISOLATION: '1',
        XDG_CONFIG_HOME: join(home, '.config'), XDG_DATA_HOME: join(home, '.local/share'), XDG_CACHE_HOME: join(home, '.cache'),
        GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' }, stdio: ['ignore', 'pipe', 'pipe'],
    });
    let diagnostics = '';
    this.child.stderr!.on('data', data => { diagnostics = (diagnostics + String(data)).slice(-6000); });
    const seed = await new Promise<string>((ok, fail) => {
      let output = '';
      const timer = setTimeout(() => fail(new Error('terminal geometry daemon readiness timed out: ' + diagnostics)), 30_000);
      this.child!.stdout!.on('data', data => { output += String(data); if (output.includes('\n')) { clearTimeout(timer); ok(output.trim().split('\n').find(line => line.startsWith('{'))!); } });
      this.child!.once('exit', () => { clearTimeout(timer); fail(new Error('terminal geometry daemon exited: ' + diagnostics)); });
    });
    const { build } = require('esbuild') as typeof import('esbuild');
    const bundle = join(this.directory, 'bundle.js');
    await build({ stdin: { contents: entry, resolveDir: resolve(root, 'web'), loader: 'tsx', sourcefile: 'terminal-geometry.tsx' },
      bundle: true, format: 'esm', outfile: bundle, tsconfig: resolve(root, 'web/tsconfig.json'),
      jsx: 'automatic', loader: { '.module.css': 'local-css' }, logLevel: 'silent' });
    const wasm = require.resolve('@wterm/ghostty/ghostty-vt.wasm');
    this.server = createServer(async (req, res) => {
      res.setHeader('Cache-Control', 'no-store');
      const path = new URL(req.url!, origin).pathname;
      if (path === '/seed.json') { res.setHeader('Content-Type', 'application/json'); res.end(seed); return; }
      const files: Record<string, [string, string]> = { '/bundle.js': [bundle, 'application/javascript'], '/bundle.css': [bundle.replace(/\.js$/, '.css'), 'text/css'], '/ghostty.wasm': [wasm, 'application/wasm'] };
      if (files[path]) { res.setHeader('Content-Type', files[path][1]); res.end(await readFile(files[path][0])); return; }
      if (path === '/layout.css') { res.setHeader('Content-Type', 'text/css'); res.end(css); return; }
      res.setHeader('Content-Type', 'text/html');
      res.setHeader('Content-Security-Policy', contentSecurityPolicy(origin));
      res.end('<!doctype html><title>Terminal geometry transport fixture</title><link rel="stylesheet" href="/layout.css"><link rel="stylesheet" href="/bundle.css"><nav>' + ['layout','hide','invisible','font','zoom','parallel','reconnect'].map(id => '<button id="' + id + '">' + id + '</button>').join('') + '</nav><main id="root"></main><script type="module" src="/bundle.js"></script>');
    });
    await new Promise<void>((ok, fail) => { this.server!.once('error', fail); this.server!.listen(3000, '127.0.0.1', ok); });
  }
  async stop() {
    if (this.server) { this.server.closeAllConnections(); await new Promise<void>(ok => this.server!.close(() => ok())); }
    if (this.child && this.child.exitCode === null) {
      const child = this.child;
      await new Promise<void>(ok => { const timer = setTimeout(() => child.kill('SIGKILL'), 3000); child.once('exit', () => { clearTimeout(timer); ok(); }); child.kill('SIGTERM'); });
    }
    if (this.directory) await rm(this.directory, { recursive: true, force: true });
  }
}

export const test = base.extend<{}, { geometryServer: void }>({
  geometryServer: [async ({}, use) => { const fixture = new GeometryServer(); try { await fixture.start(); await use(); } finally { await fixture.stop(); } }, { scope: 'worker', auto: true, timeout: 300_000 }],
});

/** Independent expectation from the actual viewport and probe, not the hook. */
export async function renderedGrid(page: Page, index = 0) {
  return page.getByRole('log', { name: `Agent ${index} output`, includeHidden: true }).evaluate(screen => {
    const viewport = screen.parentElement!;
    const probe = viewport.querySelector('span[aria-hidden]')!.getBoundingClientRect();
    const box = viewport.getBoundingClientRect();
    return { cols: Math.floor(box.width / (probe.width / 10)), rows: Math.floor(box.height / probe.height) };
  });
}
