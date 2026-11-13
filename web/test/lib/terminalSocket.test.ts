import { afterEach, describe, expect, it, vi } from 'vitest';
import type { DaemonClientDeps } from '@/lib/client/daemonClient';
import { clearGrant, setGrant, takeAuthorizationEnd, type Grant } from '@/lib/client/grant';
import type { RecoveryOutcome } from '@/lib/client/reauth';
import { decodeBase64 } from '@/lib/client/base64';
import { RESIZE_DEBOUNCE_MS, TerminalConnection, type TerminalErrorEvent } from '@/lib/client/terminalSocket';
import type { TerminalFrame } from '@/lib/client/types';
import { GRANT, SocketHub, TICKET, fixture, flush, ticketClient } from './helpers';

const fx = fixture<{ server: TerminalFrame[]; decoded: Record<string, string> }>('terminal-frames.json');

class Timers {
  queue: { fn: () => void; ms: number; id: number }[] = [];
  private id = 0;
  set = (fn: () => void, ms: number) => {
    this.queue.push({ fn, ms, id: ++this.id });
    return this.id;
  };
  clear = (handle: unknown) => {
    this.queue = this.queue.filter(t => t.id !== handle);
  };
  async runAll() {
    const due = this.queue;
    this.queue = [];
    for (const t of due) t.fn();
    await flush();
  }
}

async function setup(opts: { readOnly?: boolean } = {}) {
  const hub = new SocketHub();
  const timers = new Timers();
  const client = ticketClient();
  const connection = new TerminalConnection(client, 'term_42', opts, { socketFactory: hub.factory, setTimer: timers.set, clearTimer: timers.clear });
  await flush();
  return { hub, timers, connection, client };
}

describe('TerminalConnection', () => {
  it('opens a ticketed awman.terminal.v1 socket', async () => {
    const { hub, connection } = await setup();
    expect(hub.last.url).toBe('ws://127.0.0.1:9877/v1/terminals/term_42');
    expect(hub.last.protocols).toEqual(['awman.terminal.v1', `awman.ticket.${TICKET}`]);
    hub.last.open('awman.terminal.v1');
    expect(connection.state.get()).toBe('open');
    expect(connection.control.get()).toEqual({ holder: 'none', inputAllowed: true });
  });

  it('decodes snapshot and output frames (UTF-8 split across frames stays bytes)', async () => {
    const { hub, connection } = await setup();
    const snapshots: { cols: number; rows: number; hasOutput: boolean }[] = [];
    const outputs: Uint8Array[] = [];
    connection.onSnapshot(s => snapshots.push(s));
    connection.onOutput(bytes => outputs.push(bytes));
    hub.last.open('awman.terminal.v1');
    for (const frame of fx.server.filter(f => f.type === 'snapshot' || f.type === 'output').slice(0, 3)) hub.last.receive(frame);
    expect(snapshots[0]).toMatchObject({ hasOutput: expect.any(Boolean) });
    const first = fx.server.find(f => f.type === 'snapshot') as Extract<TerminalFrame, { type: 'snapshot' }>;
    expect(snapshots[0]).toMatchObject({ cols: first.cols, rows: first.rows });
    const joined = new Uint8Array(outputs.reduce((n, o) => n + o.length, 0));
    let offset = 0;
    for (const o of outputs) {
      joined.set(o, offset);
      offset += o.length;
    }
    const expected = fx.server.filter(f => f.type === 'output').slice(0, 2).map(f => decodeBase64((f as { bytes: string }).bytes));
    expect(joined.length).toBe(expected.reduce((n, e) => n + e.length, 0));
  });

  it('refuses input until this viewer holds control; never echoes locally', async () => {
    const { hub, connection } = await setup();
    const errors: TerminalErrorEvent[] = [];
    connection.onError(e => errors.push(e));
    hub.last.open('awman.terminal.v1');
    connection.input(new TextEncoder().encode('ls\r'));
    expect(errors.map(e => e.code)).toEqual(['not_holding']);
    expect(hub.last.sent).toEqual([]);
  });

  it('takes control, then sends input in ≤64 KiB chunks and refuses >1 MiB', async () => {
    const { hub, timers, connection } = await setup();
    const errors: string[] = [];
    connection.onError(e => errors.push(e.code));
    hub.last.open('awman.terminal.v1');
    const taken = connection.takeControl();
    expect(hub.last.frames()).toEqual([{ type: 'take_control' }]);
    await timers.runAll();
    await expect(taken).resolves.toBeUndefined();
    expect(connection.control.get().holder).toBe('me');
    connection.input(new Uint8Array(150 * 1024).fill(97));
    const inputs = hub.last.frames().filter(f => f.type === 'input');
    expect(inputs).toHaveLength(3);
    expect(inputs.map(f => decodeBase64(f.bytes as string).length)).toEqual([65536, 65536, 22528]);
    connection.input(new Uint8Array(1024 * 1024 + 1));
    expect(errors).toContain('input_too_large');
  });

  it('a control_held refusal rejects takeControl and shows another holder', async () => {
    const { hub, connection } = await setup();
    hub.last.open('awman.terminal.v1');
    const taken = connection.takeControl();
    hub.last.receive({ type: 'error', code: 'control_held', error: 'another viewer holds terminal control' });
    await expect(taken).rejects.toThrow(/another viewer/);
    expect(connection.control.get()).toEqual({ holder: 'other', inputAllowed: true });
  });

  it('read-only viewers cannot take control', async () => {
    const { hub, connection } = await setup({ readOnly: true });
    hub.last.open('awman.terminal.v1');
    await expect(connection.takeControl()).rejects.toThrow(/read-only/);
    expect(connection.control.get().inputAllowed).toBe(false);
    expect(hub.last.sent).toEqual([]);
  });

  it('debounces resize and sends it only while holding control', async () => {
    const { hub, timers, connection } = await setup();
    hub.last.open('awman.terminal.v1');
    connection.resize(100, 30);
    await timers.runAll();
    expect(hub.last.sent).toEqual([]);
    const taken = connection.takeControl();
    await timers.runAll();
    await taken;
    connection.resize(90, 30);
    connection.resize(100, 31);
    connection.resize(120, 40);
    expect(timers.queue.filter(t => t.ms === RESIZE_DEBOUNCE_MS)).toHaveLength(1);
    await timers.runAll();
    expect(hub.last.frames().filter(f => f.type === 'resize')).toEqual([{ type: 'resize', cols: 120, rows: 40 }]);
  });

  it('reconnects with a new ticket and drops control; exit is final', async () => {
    const { hub, timers, connection, client } = await setup();
    hub.last.open('awman.terminal.v1');
    const taken = connection.takeControl();
    await timers.runAll();
    await taken;
    hub.last.serverClose(1006);
    expect(connection.state.get()).toBe('reconnecting');
    expect(connection.control.get().holder).toBe('none');
    await timers.runAll();
    expect(client.ticketCalls).toBe(2);
    hub.last.open('awman.terminal.v1');
    let code: number | null = null;
    connection.onExit(c => (code = c));
    hub.last.receive({ type: 'exit', code: 3 });
    expect(code).toBe(3);
    expect(connection.state.get()).toBe('exited');
  });

  it('malformed frames close the socket and reconnect', async () => {
    const { hub, connection } = await setup();
    hub.last.open('awman.terminal.v1');
    hub.last.receive('{bad');
    expect(hub.sockets[0]!.closed).not.toBeNull();
    expect(connection.state.get()).toBe('reconnecting');
  });

  it('an unexplained 1008 on a keyless grant is classified, then ends the terminal', async () => {
    const second = await setup();
    second.hub.last.open('awman.terminal.v1');
    second.hub.last.serverClose(1008);
    expect(second.connection.state.get()).toBe('reconnecting');
    await new Promise(resolve => setTimeout(resolve, 300));
    expect(second.client.isRevoked).toBe(true);
    expect(second.connection.state.get()).toBe('closed');
    expect(second.hub.sockets).toHaveLength(1);
  });

  it('close() detaches politely', async () => {
    const { hub, connection } = await setup();
    hub.last.open('awman.terminal.v1');
    const socket = hub.last;
    connection.close();
    expect(socket.frames()).toEqual([{ type: 'detached', reason: 'user' }]);
    expect(socket.closed?.code).toBe(1000);
  });
});

// SR-09: terminal-only ending reasons. Each case has no frontend socket, so the
// terminal's own close is the only evidence the client receives.
describe('TerminalConnection policy close (1008) classification', () => {
  const BOUND: Grant = { ...GRANT, reauth: { credentialId: 'Q'.repeat(42) + 'E', daemonEpoch: 'Gs9c1yhW-SA6P6HLQUsNew', listenerEpoch: 'C0tOZXEBuALBXtZAsgmDCg' } };
  afterEach(() => {
    clearGrant();
    takeAuthorizationEnd();
  });

  async function bound(outcome: RecoveryOutcome) {
    const hub = new SocketHub();
    const timers = new Timers();
    const recover = vi.fn(async () => outcome);
    const forgetCredential = vi.fn(async () => undefined);
    const deps: DaemonClientDeps = { recover, forgetCredential, unexplainedCloseDelayMs: 0 };
    setGrant(BOUND);
    const client = ticketClient([], BOUND, deps);
    const ended: string[] = [];
    client.onAuthEnded(state => ended.push(state));
    const connection = new TerminalConnection(client, 'term_42', {}, { socketFactory: hub.factory, setTimer: timers.set, clearTimer: timers.clear });
    await flush();
    hub.last.open('awman.terminal.v1');
    return { hub, timers, connection, client, recover, forgetCredential, ended };
  }

  it.each([
    ['grant revoked', 'authorization_revoked', true],
    ['grant expired', 'authorization_expired', true],
    ['origin removed', 'origin_removed', true],
    ['listener closed', 'listener_closed', true],
    ['daemon stopping', 'daemon_stopped', false],
  ] as const)('close reason %j ends with %s at once, without a proof', async (reason, state, forgets) => {
    const h = await bound({ kind: 'granted', grant: BOUND });
    h.hub.last.serverClose(1008, reason);
    await flush();
    expect(h.ended).toEqual([state]);
    expect(h.client.endState).toBe(state);
    expect(h.recover).not.toHaveBeenCalled();
    expect(h.forgetCredential).toHaveBeenCalledTimes(forgets ? 1 : 0);
    expect(h.connection.state.get()).toBe('closed');
    expect(h.connection.control.get()).toEqual({ holder: 'none', inputAllowed: false });
    await h.timers.runAll();
    expect(h.hub.sockets).toHaveLength(1);
    expect(h.client.ticketCalls).toBe(1);
  });

  it('an unexplained 1008 disproven by one proof is an ordinary reconnect (new ticket, control not re-taken)', async () => {
    const h = await bound({ kind: 'granted', grant: BOUND });
    void h.connection.takeControl();
    expect(h.connection.control.get().holder).toBe('me');
    h.hub.last.serverClose(1008, 'policy');
    expect(h.connection.state.get()).toBe('reconnecting');
    await new Promise(resolve => setTimeout(resolve, 0));
    await flush();
    expect(h.recover).toHaveBeenCalledTimes(1);
    expect(h.ended).toEqual([]);
    expect(h.client.isRevoked).toBe(false);
    expect(h.connection.state.get()).toBe('reconnecting');
    await h.timers.runAll();
    expect(h.client.ticketCalls).toBe(2);
    expect(h.hub.sockets).toHaveLength(2);
    h.hub.last.open('awman.terminal.v1');
    expect(h.connection.state.get()).toBe('open');
    expect(h.connection.control.get()).toEqual({ holder: 'none', inputAllowed: true });
    expect(h.hub.last.sent).toEqual([]);
  });

  it('an unexplained 1008 whose proof reports revocation ends with that verified state', async () => {
    const h = await bound({ kind: 'ended', state: 'authorization_revoked' });
    h.hub.last.serverClose(1008, '');
    await new Promise(resolve => setTimeout(resolve, 0));
    await flush();
    expect(h.recover).toHaveBeenCalledTimes(1);
    expect(h.ended).toEqual(['authorization_revoked']);
    expect(h.connection.state.get()).toBe('closed');
    await h.timers.runAll();
    expect(h.hub.sockets).toHaveLength(1);
  });
});
