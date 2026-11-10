// Shared terminal test helpers: a core factory that loads the real Ghostty
// WASM from node_modules, a fake TerminalConnection, and fixture loading.
import { readFileSync, readdirSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { Emitter, Subject } from '@/lib/client/observable';
import type { TerminalConnection, TerminalConnectionState, TerminalControl, TerminalErrorEvent, TerminalSnapshot } from '@/lib/client/terminalSocket';
import type { CoreFactory } from '@/terminal/core/loadCore';

const require = createRequire(import.meta.url);
export const GHOSTTY_WASM = require.resolve('@wterm/ghostty/ghostty-vt.wasm');
const wasmUrl = pathToFileURL(GHOSTTY_WASM).href;

/** Node fetch cannot read file: URLs; serve the one WASM file the core asks for. */
async function withFileFetch<T>(run: () => Promise<T>): Promise<T> {
  const original = globalThis.fetch;
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input instanceof Request ? input.url : input);
    if (url !== wasmUrl) throw new Error(`unexpected fetch in terminal test: ${url}`);
    return new Response(readFileSync(fileURLToPath(url)), { headers: { 'Content-Type': 'application/wasm' } });
  }) as typeof fetch;
  try {
    return await run();
  } finally {
    globalThis.fetch = original;
  }
}

export const nodeCoreFactory: CoreFactory = async ({ cols, rows, scrollbackLines }) => {
  const { GhosttyCore } = await import('@wterm/ghostty');
  const core = await withFileFetch(() => GhosttyCore.load({ wasmPath: wasmUrl, scrollbackLines, imageStorageLimit: 0 }));
  core.init(cols, rows);
  return core;
};

export class FakeConnection {
  readonly terminalId = 'term-1';
  readonly stateSubject = new Subject<TerminalConnectionState>('open');
  readonly state = this.stateSubject;
  readonly controlSubject: Subject<TerminalControl>;
  readonly control: Subject<TerminalControl>;
  readonly snapshots = new Emitter<TerminalSnapshot>();
  readonly outputs = new Emitter<Uint8Array>();
  readonly resizes = new Emitter<{ cols: number; rows: number }>();
  readonly exits = new Emitter<number>();
  readonly errors = new Emitter<TerminalErrorEvent>();
  readonly inputs: Uint8Array[] = [];
  readonly resizeCalls: { cols: number; rows: number }[] = [];
  takeControlCalls = 0;

  constructor(control: TerminalControl = { holder: 'none', inputAllowed: true }) {
    this.controlSubject = new Subject<TerminalControl>(control);
    this.control = this.controlSubject;
  }

  onSnapshot(cb: (s: TerminalSnapshot) => void) {
    return this.snapshots.on(cb);
  }
  onOutput(cb: (b: Uint8Array) => void) {
    return this.outputs.on(cb);
  }
  onResize(cb: (c: number, r: number) => void) {
    return this.resizes.on(s => cb(s.cols, s.rows));
  }
  onExit(cb: (code: number) => void) {
    return this.exits.on(cb);
  }
  onError(cb: (e: TerminalErrorEvent) => void) {
    return this.errors.on(cb);
  }
  input(bytes: Uint8Array) {
    if (this.controlSubject.get().holder !== 'me') throw new Error('input sent without holding control');
    this.inputs.push(bytes.slice());
  }
  resize(cols: number, rows: number) {
    if (this.controlSubject.get().holder !== 'me') throw new Error('resize sent without holding control');
    this.resizeCalls.push({ cols, rows });
  }
  takeControl() {
    this.takeControlCalls += 1;
    this.controlSubject.set({ holder: 'me', inputAllowed: true });
    return Promise.resolve();
  }
  close() {
    this.stateSubject.set('closed');
  }

  /** Feed a fixture frame exactly as TerminalConnection.dispatch would. */
  frame(frame: FixtureFrame) {
    if (frame.type === 'snapshot') this.snapshots.emit({ cols: frame.cols, rows: frame.rows, screen: b64(frame.screen), hasOutput: frame.has_output ?? true });
    else if (frame.type === 'output') this.outputs.emit(b64(frame.bytes));
    else this.resizes.emit({ cols: frame.cols, rows: frame.rows });
  }

  asConnection(): TerminalConnection {
    return this as unknown as TerminalConnection;
  }

  sent(): string {
    return this.inputs.map(bytes => Buffer.from(bytes).toString('latin1')).join('');
  }
}

export type FixtureFrame =
  | { type: 'snapshot'; cols: number; rows: number; screen: string; has_output?: boolean }
  | { type: 'output'; bytes: string }
  | { type: 'resize'; cols: number; rows: number };

export interface Expectation {
  lines: string[];
  cursor: { row: number; col: number; visible: boolean };
  alt_screen: boolean;
  app_cursor: boolean;
  bracketed_paste: boolean;
  mouse_mode?: string;
  mouse_sgr?: boolean;
  cols?: number;
  rows?: number;
}

export interface Fixture {
  name: string;
  about: string;
  frames: FixtureFrame[];
  final_snapshot: Extract<FixtureFrame, { type: 'snapshot' }>;
  expect: Expectation;
  expect_from_snapshot: Expectation;
  generated_output?: 'numbered_lines_10k';
}

export const FIXTURE_DIR = join(dirname(require.resolve('../../package.json')), 'src/terminal/fixtures/');

export function loadFixtures(): Fixture[] {
  return readdirSync(FIXTURE_DIR)
    .filter(name => name.endsWith('.json'))
    .sort()
    .map(name => JSON.parse(readFileSync(`${FIXTURE_DIR}${name}`, 'utf8')) as Fixture);
}

export function fixture(name: string): Fixture {
  const found = loadFixtures().find(f => f.name === name);
  if (!found) throw new Error(`no fixture ${name}`);
  return found;
}

/** The output the scrollback_10k fixture was generated from (see generator/main.rs.txt). */
export function numberedLinesFrames(): FixtureFrame[] {
  let text = '';
  for (let i = 0; i < 10_000; i++) text += `line ${String(i).padStart(5, '0')} \x1b[2m${'x'.repeat(i % 50)}\x1b[0m\r\n`;
  const bytes = Buffer.from(text, 'utf8');
  const frames: FixtureFrame[] = [];
  for (let offset = 0; offset < bytes.length; offset += 65_536) frames.push({ type: 'output', bytes: bytes.subarray(offset, offset + 65_536).toString('base64') });
  return frames;
}

export function b64(text: string): Uint8Array {
  return Uint8Array.from(Buffer.from(text, 'base64'));
}

export const syncSchedule = (paint: () => void) => paint();
