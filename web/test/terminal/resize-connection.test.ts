import { afterEach, describe, expect, it, vi } from 'vitest';
import { RESIZE_DEBOUNCE_MS, TerminalConnection } from '@/lib/client/terminalSocket';
import { flush, SocketHub, ticketClient } from '../lib/helpers';

afterEach(() => vi.useRealTimers());
async function setup(readOnly = false) {
  vi.useFakeTimers();
  const hub = new SocketHub();
  const connection = new TerminalConnection(ticketClient(), 'term_geometry', { readOnly }, { socketFactory: hub.factory });
  await flush();
  hub.last.open('awman.terminal.v1');
  return { hub, connection };
}
async function own(connection: TerminalConnection) {
  const taking = connection.takeControl();
  await vi.advanceTimersByTimeAsync(300);
  await taking;
}
const resizes = (hub: SocketHub) => hub.last.frames().filter(f => f.type === 'resize');

describe('terminal resize ownership and debounce', () => {
  it('debounces a storm to its final dimensions; duplicate measurements do not starve it', async () => {
    const { hub, connection } = await setup();
    await own(connection);
    connection.resize(90, 30);
    await vi.advanceTimersByTimeAsync(100);
    connection.resize(120, 40);
    await vi.advanceTimersByTimeAsync(100);
    connection.resize(120, 40);
    expect(resizes(hub)).toEqual([]);
    await vi.advanceTimersByTimeAsync(50);
    expect(resizes(hub)).toEqual([{ type: 'resize', cols: 120, rows: 40 }]);
    connection.close();
  });
  it('does not retain a viewer measurement for a later grant', async () => {
    const { hub, connection } = await setup();
    connection.resize(90, 30);
    await own(connection);
    expect(resizes(hub)).toEqual([]);
    connection.close();
    const viewer = await setup(true);
    expect(viewer.hub.last.url).toMatch(/read_only=true$/);
    viewer.connection.resize(130, 40);
    await vi.advanceTimersByTimeAsync(RESIZE_DEBOUNCE_MS);
    expect(resizes(viewer.hub)).toEqual([]);
    viewer.connection.close();
  });
  it('drops pending measurements on hide/cancel, refusal, disconnect and close', async () => {
    for (const transition of ['cancel', 'refuse', 'disconnect', 'close']) {
      const { hub, connection } = await setup();
      await own(connection);
      connection.resize(120, 40);
      const oldSocket = hub.last;
      if (transition === 'cancel') connection.cancelResize();
      if (transition === 'refuse') hub.last.receive({ type: 'error', code: 'input_not_allowed', error: 'revoked' });
      if (transition === 'disconnect') hub.last.serverClose(1006);
      if (transition === 'close') connection.close();
      await vi.advanceTimersByTimeAsync(RESIZE_DEBOUNCE_MS);
      expect(oldSocket.frames().filter(f => f.type === 'resize')).toEqual([]);
      connection.close();
    }
  });
  it('rejects non-finite, zero, fractional and out-of-protocol dimensions', async () => {
    const { hub, connection } = await setup();
    await own(connection);
    for (const value of [0, -1, NaN, Infinity, 1.5, 1001]) {
      connection.resize(value, 30);
      connection.resize(100, value);
    }
    await vi.advanceTimersByTimeAsync(RESIZE_DEBOUNCE_MS);
    expect(resizes(hub)).toEqual([]);
    connection.close();
  });
  it('keeps parallel terminals independent and requires a fresh measurement after reconnect', async () => {
    const first = await setup();
    const second = await setup();
    await own(first.connection);
    await own(second.connection);
    first.connection.resize(100, 30);
    second.connection.resize(70, 20);
    first.hub.last.serverClose(1006);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(resizes(second.hub)).toEqual([{ type: 'resize', cols: 70, rows: 20 }]);
    first.hub.last.open('awman.terminal.v1');
    await own(first.connection);
    expect(resizes(first.hub)).toEqual([]);
    first.connection.resize(110, 35);
    await vi.advanceTimersByTimeAsync(RESIZE_DEBOUNCE_MS);
    expect(resizes(first.hub)).toEqual([{ type: 'resize', cols: 110, rows: 35 }]);
    first.connection.close(); second.connection.close();
  });
});
