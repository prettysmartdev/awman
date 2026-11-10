import { describe, expect, it } from 'vitest';
import { encodeBase64 } from '@/lib/client/base64';
import { GrantRevoked } from '@/lib/client/errors';
import { BackpressureError, FrontendConnection, NotConnectedError, type HelloOptions } from '@/lib/client/frontendSocket';
import type { EngineFrame, InstanceSnapshot } from '@/lib/client/types';
import type { FakeSocket} from './helpers';
import { SocketHub, TICKET, fixture, flush, ticketClient } from './helpers';

const frames = fixture<(EngineFrame & { seq: number })[]>('engine-frames.json');
const welcome = frames.find(f => f.type === 'welcome')!;
const snapshotFrame = frames.find(f => f.type === 'snapshot')!;
const { type: _t, seq: _s, ...SNAPSHOT_BODY } = snapshotFrame;
const SNAPSHOT = SNAPSHOT_BODY as unknown as InstanceSnapshot;
const TAB = SNAPSHOT.tabs[0]!.tab_id;

class Clock {
  now = 1_000_000;
  timers: { at: number; fn: () => void; id: number }[] = [];
  private nextId = 0;
  setTimer = (fn: () => void, ms: number) => {
    const id = ++this.nextId;
    this.timers.push({ at: this.now + ms, fn, id });
    return id;
  };
  clearTimer = (handle: unknown) => {
    this.timers = this.timers.filter(t => t.id !== handle);
  };
  async advance(ms: number) {
    this.now += ms;
    const due = this.timers.filter(t => t.at <= this.now);
    this.timers = this.timers.filter(t => t.at > this.now);
    for (const t of due) t.fn();
    await flush();
  }
}

async function setup(hello: Partial<HelloOptions> = {}, client = ticketClient()) {
  const hub = new SocketHub();
  const clock = new Clock();
  const connection = new FrontendConnection(
    client,
    SNAPSHOT.instance_id,
    { role: 'primary', launchContext: 'lc_abc', termSize: { cols: 120, rows: 40 }, ...hello },
    { socketFactory: hub.factory, now: () => clock.now, setTimer: clock.setTimer, clearTimer: clock.clearTimer },
  );
  await flush();
  return { hub, clock, connection, client };
}

/** Server side of a successful attach: welcome (seq 1) + snapshot (seq 2). */
function attach(socket: FakeSocket, frontendId = 'frn_1') {
  socket.open('awman.frontend.v1');
  socket.receive({ ...welcome, seq: 1, frontend_id: frontendId });
  socket.receive({ ...SNAPSHOT_BODY, type: 'snapshot', seq: 2 });
}

describe('FrontendConnection: handshake', () => {
  it('opens a ticketed socket and sends a web Hello without env/cwd/argv', async () => {
    const { hub, connection } = await setup();
    const socket = hub.last;
    expect(socket.url).toBe(`ws://127.0.0.1:9877/v1/engine/instances/${SNAPSHOT.instance_id}/frontend`);
    expect(socket.protocols).toEqual(['awman.frontend.v1', `awman.ticket.${TICKET}`]);
    expect(socket.url).not.toContain(TICKET);
    socket.open('awman.frontend.v1');
    const hello = socket.frames()[0]!;
    expect(hello).toMatchObject({ seq: 1, type: 'hello', protocol: 'awman.frontend.v1', kind: 'web', role: 'primary', cwd: '', argv: [], env: {}, launch_context: 'lc_abc', term_size: { cols: 120, rows: 40 } });
    expect(hello).not.toHaveProperty('resume');
    expect(connection.state.get()).toBe('connecting');
  });

  it('is open after welcome + snapshot and numbers outbound frames from 2', async () => {
    const { hub, connection } = await setup();
    attach(hub.last);
    expect(connection.state.get()).toBe('open');
    expect(connection.id).toBe('frn_1');
    expect(connection.role.get()).toBe('primary');
    expect(connection.store.get().snapshot?.instance_id).toBe(SNAPSHOT.instance_id);
    expect(connection.send({ type: 'activity', tab_id: TAB })).toBe(2);
    expect(connection.send({ type: 'focus_tab', tab_id: TAB })).toBe(3);
  });

  it('refuses a server that selects the wrong subprotocol (e.g. echoes the ticket)', async () => {
    const { hub, connection } = await setup();
    hub.last.open(`awman.ticket.${TICKET}`);
    expect(hub.last.closed?.code).toBe(4002);
    expect(connection.state.get()).toBe('reconnecting');
  });

  it('a secondary sends its view and never a launch context', async () => {
    const { hub } = await setup({ role: 'secondary', view: 'all', launchContext: 'lc_abc' });
    hub.last.open('awman.frontend.v1');
    const hello = hub.last.frames()[0]!;
    expect(hello).toMatchObject({ role: 'secondary', view: 'all' });
    expect(hello).not.toHaveProperty('launch_context');
  });
});

describe('FrontendConnection: ordering and resync', () => {
  it('applies ordered deltas to the store', async () => {
    const { hub, connection } = await setup();
    attach(hub.last);
    hub.last.receive({ seq: 3, type: 'delta', delta: 'stuck', value: { tab_id: TAB, stuck: true } });
    expect(connection.store.get().snapshot?.tabs[0]?.stuck).toBe(true);
  });

  it('a sequence gap closes, fetches a NEW ticket and resumes with a full resync', async () => {
    const { hub, clock, connection, client } = await setup();
    attach(hub.last);
    hub.last.receive({ seq: 5, type: 'notice', level: 'info', text: 'skipped 3 and 4' });
    expect(hub.sockets[0]!.closed?.code).toBe(4002);
    expect(connection.state.get()).toBe('reconnecting');
    await clock.advance(50);
    expect(client.ticketCalls).toBe(2);
    expect(hub.sockets).toHaveLength(2);
    hub.last.open('awman.frontend.v1');
    const hello = hub.last.frames()[0]!;
    expect(hello).toMatchObject({ seq: 1, type: 'hello', resume: 'frn_1' });
    expect(hello).not.toHaveProperty('launch_context');
    const generation = connection.store.get().generation;
    attach(hub.last);
    expect(connection.state.get()).toBe('open');
    expect(connection.store.get().generation).toBe(generation + 1);
  });

  it('rejects unknown frame types, a non-welcome first frame, binary and oversized frames', async () => {
    for (const bad of [
      (s: FakeSocket) => s.receive({ seq: 1, type: 'welcome_v2' }),
      (s: FakeSocket) => s.receive({ seq: 1, type: 'notice', level: 'info', text: 'x' }),
      (s: FakeSocket) => s.onmessage?.({ data: new ArrayBuffer(4) }),
      (s: FakeSocket) => s.receive('x'.repeat(1024 * 1024 + 1)),
      (s: FakeSocket) => s.receive('{not json'),
    ]) {
      const { hub, connection } = await setup();
      hub.last.open('awman.frontend.v1');
      bad(hub.last);
      expect(hub.last.closed).not.toBeNull();
      expect(connection.state.get()).toBe('reconnecting');
    }
  });

  it('reassembles a chunked snapshot (multi-byte split across chunks)', async () => {
    const chunks = frames.filter(f => f.type === 'snapshot_chunk');
    expect(chunks.length).toBeGreaterThan(1);
    const { hub, connection } = await setup();
    hub.last.open('awman.frontend.v1');
    hub.last.receive({ ...welcome, seq: 1 });
    chunks.forEach((chunk, index) => hub.last.receive({ ...chunk, seq: index + 2 }));
    expect(connection.state.get()).toBe('open');
    expect(connection.store.get().snapshot?.instance_id).toBe(SNAPSHOT.instance_id);
  });

  it('rejects out-of-order or oversized chunk sequences', async () => {
    const { hub, connection } = await setup();
    hub.last.open('awman.frontend.v1');
    hub.last.receive({ ...welcome, seq: 1 });
    hub.last.receive({ seq: 2, type: 'snapshot_chunk', chunk: 1, of: 2, bytes: encodeBase64(new Uint8Array([1])) });
    expect(connection.state.get()).toBe('reconnecting');
  });
});

describe('FrontendConnection: runtime frame validation (review #2)', () => {
  const malformedAfterAttach: [string, Record<string, unknown>][] = [
    ['unknown delta kind', { type: 'delta', delta: 'future_kind', value: {} }],
    ['known delta, malformed value', { type: 'delta', delta: 'phase', value: { tab_id: 'x', phase: 'running' } }],
    ['known delta, missing value', { type: 'delta', delta: 'stuck' }],
    ['tab_opened with a non-tab', { type: 'delta', delta: 'tab_opened', value: { tab_id: 'x' } }],
    ['squad_health outside the enum', { type: 'delta', delta: 'squad_health', value: 'exploded' }],
    ['malformed full snapshot', { type: 'snapshot', instance_id: 'x', tabs: 'nope' }],
    ['snapshot without running_jobs', { ...SNAPSHOT_BODY, type: 'snapshot', running_jobs: undefined }],
    ['snapshot for another instance', { ...SNAPSHOT_BODY, type: 'snapshot', instance_id: 'ins_other' }],
    ['repeated welcome', { ...welcome }],
    ['notice with a bad level', { type: 'notice', level: 'shout', text: 'x' }],
    ['command_output without text', { type: 'command_output', tab_id: 'x', stream: 'stdout', level: 'info' }],
    ['dialog_request with an unknown kind', { type: 'dialog_request', dialog_id: 'd', tab_id: 'x', request: { kind: 'nope' } }],
  ];
  it.each(malformedAfterAttach)('%s: closes and resyncs instead of corrupting the snapshot', async (_name, body) => {
    const { hub, clock, connection, client } = await setup();
    attach(hub.last);
    const before = connection.store.get().snapshot;
    hub.last.receive({ ...body, seq: 3 });
    expect(hub.sockets[0]!.closed?.code).toBe(4002);
    expect(connection.state.get()).toBe('reconnecting');
    expect(connection.store.get().snapshot).toBe(before);
    expect(connection.store.get().snapshot?.tabs.length).toBeGreaterThan(0);
    await clock.advance(50);
    expect(client.ticketCalls).toBe(2);
    attach(hub.last);
    expect(connection.state.get()).toBe('open');
  });

  it.each([
    ['wrong role', { role: 'secondary' }],
    ['wrong instance', { instance_id: 'ins_other' }],
    ['wrong protocol', { protocol: 'awman.frontend.v2' }],
    ['missing frontend id', { frontend_id: '' }],
  ])('rejects a welcome with the %s', async (_name, patch) => {
    const { hub, connection } = await setup();
    hub.last.open('awman.frontend.v1');
    hub.last.receive({ ...welcome, seq: 1, ...patch });
    expect(hub.last.closed?.code).toBe(4002);
    expect(connection.role.get()).toBeNull();
    expect(connection.state.get()).toBe('reconnecting');
  });

  it('a chunked snapshot that decodes to a malformed snapshot is refused', async () => {
    const { hub, connection } = await setup();
    hub.last.open('awman.frontend.v1');
    hub.last.receive({ ...welcome, seq: 1 });
    hub.last.receive({ seq: 2, type: 'snapshot_chunk', chunk: 0, of: 1, bytes: encodeBase64(new TextEncoder().encode('{"tabs":7}')) });
    expect(connection.store.get().snapshot).toBeNull();
    expect(connection.state.get()).toBe('reconnecting');
  });

  it('accepts every frame in the engine fixture', async () => {
    const { hub, connection } = await setup();
    attach(hub.last);
    const rest = frames.filter(f => !['welcome', 'snapshot', 'snapshot_chunk', 'detach'].includes(f.type));
    rest.forEach((frame, index) => hub.last.receive({ ...frame, seq: index + 3 }));
    expect(hub.last.closed).toBeNull();
    expect(connection.state.get()).toBe('open');
  });
});

describe('FrontendConnection: never replay', () => {
  it('drops unconfirmed submits and dialog answers on disconnect and says so', async () => {
    const { hub, clock, connection } = await setup();
    attach(hub.last);
    connection.send({ type: 'submit', tab_id: TAB, input: { source: 'command_box', raw: 'exec workflow ship.toml' }, pty_size: { cols: 80, rows: 24 } });
    connection.send({ type: 'dialog_response', dialog_id: 'dlg_1', response: { kind: 'yes' } });
    hub.last.serverClose(1006);
    const notices = connection.store.get().notices.map(n => n.text).join('\n');
    expect(notices).toMatch(/Not confirmed — check the log: "exec workflow ship.toml"/);
    expect(notices).toMatch(/dialog answer was not confirmed/);
    await clock.advance(50);
    attach(hub.last);
    expect(hub.last.frames().map(f => f.type)).toEqual(['hello']);
  });

  it('confirmed work is not reported (phase delta / command_finished / dialog_closed)', async () => {
    const { hub, connection } = await setup();
    attach(hub.last);
    connection.send({ type: 'submit', tab_id: TAB, input: { source: 'command_box', raw: 'ready' }, pty_size: { cols: 80, rows: 24 } });
    connection.send({ type: 'dialog_response', dialog_id: 'dlg_1', response: { kind: 'no' } });
    hub.last.receive({ seq: 3, type: 'delta', delta: 'phase', value: { tab_id: TAB, phase: { phase: 'running', command: 'ready' }, current_workflow: null, current_container: null } });
    hub.last.receive({ seq: 4, type: 'dialog_closed', dialog_id: 'dlg_1', answered_by: { by: 'frontend', frontend_id: 'frn_1' } });
    hub.last.serverClose(1006);
    expect(connection.store.get().notices).toEqual([]);
  });

  it('send() refuses while not open and when congested; nothing is queued', async () => {
    const { hub, connection } = await setup();
    expect(() => connection.send({ type: 'quit' })).toThrow(NotConnectedError);
    attach(hub.last);
    hub.last.bufferedAmount = 5 * 1024 * 1024;
    expect(() => connection.send({ type: 'quit' })).toThrow(BackpressureError);
    expect(hub.last.frames().map(f => f.type)).toEqual(['hello']);
  });

  it('resolves uiRequest from the matching ui_ack and rejects it on disconnect', async () => {
    const { hub, connection } = await setup();
    attach(hub.last);
    const ack = connection.uiRequest({ request: 'next_tab' });
    hub.last.receive({ seq: 3, type: 'ui_ack', request_seq: 2, outcome: { claimed_by_host: false, claimed_by: null, detail: null } });
    await expect(ack).resolves.toMatchObject({ claimed_by_host: false });
    const lost = connection.uiRequest({ request: 'prev_tab' });
    hub.last.serverClose(1006);
    await expect(lost).rejects.toBeInstanceOf(NotConnectedError);
  });
});

describe('FrontendConnection: endings', () => {
  it('a bare refusal is final (no reconnect loop)', async () => {
    const { hub, clock, connection, client } = await setup();
    hub.last.open('awman.frontend.v1');
    hub.last.receive({ code: 'primary_exists', error: 'instance already has a primary' });
    hub.last.serverClose(1005);
    expect(connection.state.get()).toBe('closed');
    expect(connection.closeReason).toEqual({ kind: 'refused', code: 'primary_exists', error: 'instance already has a primary' });
    await clock.advance(5000);
    expect(client.ticketCalls).toBe(1);
  });

  it('close 1008 marks the grant revoked', async () => {
    const { hub, connection, client } = await setup();
    attach(hub.last);
    hub.last.serverClose(1008);
    expect(connection.closeReason).toEqual({ kind: 'revoked' });
    expect(client.isRevoked).toBe(true);
    expect(connection.store.get().ended).not.toBeNull();
  });

  it('a revoked grant while fetching a ticket ends the connection', async () => {
    const { connection } = await setup({}, ticketClient([() => Promise.reject(new GrantRevoked())]));
    expect(connection.closeReason).toEqual({ kind: 'revoked' });
  });

  it('gives up after the 60 s web reconnect grace and marks the instance ended', async () => {
    const failing = () => Promise.reject(new Error('daemon down'));
    const { hub, clock, connection } = await setup({}, ticketClient([() => Promise.resolve(TICKET), ...Array(100).fill(failing)]));
    attach(hub.last);
    hub.last.serverClose(1006);
    for (let i = 0; i < 70 && connection.state.get() !== 'closed'; i++) await clock.advance(1000);
    expect(connection.closeReason).toEqual({ kind: 'grace_expired' });
    expect(connection.store.get().ended?.reason).toMatch(/reconnect grace/);
  });

  it('detach from the engine closes without reconnecting', async () => {
    const { hub, clock, connection, client } = await setup();
    attach(hub.last);
    hub.last.receive({ seq: 3, type: 'detach', reason: 'instance_ended' });
    expect(connection.closeReason).toEqual({ kind: 'detached', reason: 'instance_ended' });
    await clock.advance(5000);
    expect(client.ticketCalls).toBe(1);
  });

  it('close() by the user closes the socket normally', async () => {
    const { hub, connection } = await setup();
    attach(hub.last);
    connection.close('user');
    expect(hub.sockets[0]!.closed?.code).toBe(1000);
    expect(connection.state.get()).toBe('closed');
  });
});
