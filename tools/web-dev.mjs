#!/usr/bin/env node
// Keep Next on loopback. When Tailscale is running, a TCP relay listens only
// on this machine's tailnet IPv4 address and forwards to the same dev server.
import { execFileSync, spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { connect, createServer, isIP } from 'node:net';
import { fileURLToPath } from 'node:url';

function activeTailnetIp() {
  try {
    const status = JSON.parse(execFileSync('tailscale', ['status', '--json'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 2000 }));
    if (status.BackendState !== 'Running') return null;
    const ip = execFileSync('tailscale', ['ip', '-4'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 2000 }).trim();
    return isIP(ip) === 4 ? ip : null;
  } catch { return null; }
}

async function tailnetRelay(ip) {
  const server = createServer(client => {
    const upstream = connect(3000, '127.0.0.1');
    client.on('error', () => upstream.destroy());
    upstream.on('error', () => client.destroy());
    client.pipe(upstream);
    upstream.pipe(client);
  });
  return new Promise(resolve => {
    server.once('error', error => { console.error(`Tailnet dev address ${ip}:3000 is unavailable: ${error.message}`); resolve(null); });
    server.listen(3000, ip, () => resolve(server));
  });
}

const ip = activeTailnetIp();
const relay = ip ? await tailnetRelay(ip) : null;
const webDir = fileURLToPath(new URL('../web/', import.meta.url));
const { packageManager } = JSON.parse(readFileSync(new URL('../web/package.json', import.meta.url), 'utf8'));
console.log('Web app: http://127.0.0.1:3000 (local)');
if (relay) console.log(`Web app: http://${ip}:3000 (tailnet)`);
const env = { ...process.env };
if (relay) env.WEB_DEV_TAILNET_IP = ip;
// npm exec installs the pinned pnpm into its cache and keeps it on PATH for
// Next's own SWC downloader, which calls `pnpm config get registry` on demand.
const child = spawn('npm', ['exec', '--yes', `--package=${packageManager}`, '--', 'pnpm', 'run', 'dev'], { cwd: webDir, stdio: 'inherit', env });
const stop = signal => { child.kill(signal); relay?.close(); };
process.on('SIGINT', () => stop('SIGINT'));
process.on('SIGTERM', () => stop('SIGTERM'));
child.on('error', error => { console.error(error.message); relay?.close(); process.exitCode = 1; });
child.on('exit', (code, signal) => {
  relay?.close();
  process.exitCode = code ?? (signal ? 1 : 0);
});
