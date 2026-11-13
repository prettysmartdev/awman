import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
const execute = promisify(execFile);
export const root = resolve(fileURLToPath(new URL('../..', import.meta.url)));
export const endpoint = 'http://127.0.0.1:9877';
export const origin = 'http://127.0.0.1:3000';

export class Daemon {
  private child?: ChildProcess;
  private directory = '';
  private env: NodeJS.ProcessEnv = {};
  async init() {
    // Fail before touching an existing listener; never use the developer's daemon.
    await new Promise<void>((ok, fail) => {
      const socket = createServer();
      socket.once('error', fail);
      socket.listen(9877, '127.0.0.1', () => socket.close(() => ok()));
    });
    this.directory = await mkdtemp(join(tmpdir(), 'aw159-'));
    await mkdir(join(this.directory, 'repo'));
    this.env = {
      PATH: process.env.PATH,
      HOME: this.directory,
      AWMAN_HOME: join(this.directory, 'data'),
      XDG_CONFIG_HOME: join(this.directory, '.config'),
      XDG_DATA_HOME: join(this.directory, '.local/share'),
      XDG_CACHE_HOME: join(this.directory, '.cache'),
      AWMAN_TEST_ISOLATION: '1',
      GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null',
    };
    await this.cli('config', 'set', 'daemon.features.worker.enabled', 'true');
  }
  async cli(...args: string[]) {
    const { stdout } = await execute(resolve(root, 'target/debug/awman'), args, {
      env: this.env, cwd: join(this.directory, 'repo'), timeout: 20_000,
    });
    return stdout;
  }
  async set(field: string, value: string) {
    await this.cli('config', 'set', field, value);
  }
  async recipe() {
    await this.set('daemon.features.engine.enabled', 'true');
    await this.set('daemon.network.listeners.loopback.enabled', 'true');
  }
  async allowOrigin() {
    await this.set('daemon.network.listeners.loopback.allowedOrigins', origin);
  }
  async start() {
    this.child = spawn(resolve(root, 'target/debug/awman'), ['daemon', 'start'], {
      env: this.env, cwd: join(this.directory, 'repo'), stdio: 'ignore',
    });
    for (let i = 0; i < 100; i++) {
      if (this.child.exitCode !== null) throw new Error('isolated daemon exited before readiness');
      try { await this.status(); return; } catch { await new Promise(r => setTimeout(r, 100)); }
    }
    throw new Error('isolated daemon failed to start');
  }
  /**
   * Make the sandbox repo a Git repo and register it as the default launch
   * context. A new workspace then starts `ready`, which waits on a real
   * yes/no dialog (no Dockerfile.dev yet) without needing Docker.
   */
  async launchContext() {
    await execute('git', ['init', '-q'], { env: this.env, cwd: join(this.directory, 'repo'), timeout: 20_000 });
    await this.cli('daemon', 'api', 'launch_contexts.create', 'default=true');
  }
  /** Owner view of browser grants (`GET /v1/browsers`). */
  async browsers(): Promise<{ id: string; origin: string; listener: string; reauth?: { credential_id: string; key_sha256: string } }[]> {
    return JSON.parse(await this.cli('daemon', 'api', '--raw', 'GET', '/v1/browsers', '--json')).items;
  }
  /** Owner revocation (`browsers.delete`). */
  async revoke(id: string) {
    await this.cli('daemon', 'api', 'browsers.delete', `id=${id}`);
  }
  /** Graceful stop, then a new daemon process on the same private state. */
  async restart() {
    await this.stop();
    await this.start();
  }
  async status() {
    return JSON.parse(await this.cli('daemon', 'api', '--raw', 'GET', '/v1/status', '--json'));
  }
  async confirmLink(code: string, approve: boolean) {
    if (!/^web-[A-Z0-9]{4}-[A-Z0-9]{4}$/.test(code)) throw new Error('invalid browser code');
    // util-linux script supplies a real PTY; never add a --yes bypass. Write
    // the answer only after the CLI has displayed its approval prompt.
    const quote = (text: string) => "'" + text.replaceAll("'", "'\\''") + "'";
    const command = [resolve(root, 'target/debug/awman'), 'link', code].map(quote).join(' ');
    return new Promise<string>((ok, fail) => {
      const child = spawn('script', ['-q', '-e', '-c', command, '/dev/null'], {
        env: this.env, cwd: join(this.directory, 'repo'), stdio: 'pipe',
      });
      let output = '', answered = false;
      const timer = setTimeout(() => { child.kill('SIGKILL'); fail(new Error('browser link CLI did not finish')); }, 20_000);
      child.stdout.on('data', data => {
        output += String(data);
        if (!answered && output.includes('Approve this browser?')) {
          answered = true;
          child.stdin.write(approve ? 'y\n' : 'n\n');
        }
      });
      child.once('error', error => { clearTimeout(timer); fail(error); });
      child.once('exit', code => {
        clearTimeout(timer);
        if (code === 0 && answered) ok(output);
        else fail(new Error('browser link CLI failed before explicit confirmation completed'));
      });
    });
  }
  async stop() {
    const child = this.child;
    this.child = undefined;
    if (child && child.exitCode === null) {
      await new Promise<void>(done => {
        const timer = setTimeout(() => child.kill('SIGKILL'), 3000);
        child.once('exit', () => { clearTimeout(timer); done(); });
        child.kill('SIGTERM');
      });
    }
  }
  async dispose() {
    await this.stop();
    if (this.directory) await rm(this.directory, { recursive: true, force: true });
  }
}
