import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { DaemonClient } from '@/lib/client/daemonClient';
import type { Grant } from '@/lib/client/grant';
import { HttpTransport, type FetchLike } from '@/lib/client/http';
import type { WebSocketLike } from '@/lib/client/socket';

export function fixture<T>(name: string): T {
  return JSON.parse(readFileSync(join(__dirname, 'fixtures', name), 'utf8')) as T;
}

export const GRANT: Grant = {
  id: 'brw_0123456789abcdef0123456789abcdef',
  token: `awb_${'A'.repeat(43)}`,
  scope: 'engine',
  origin: 'https://awman.prettysmart.dev',
  listener: 'loopback',
  expiresAt: '2999-01-01T00:00:00Z',
};

export const TICKET = `awt_${'T'.repeat(43)}`;

export class FakeSocket implements WebSocketLike {
  readyState = 0;
  protocol = '';
  bufferedAmount = 0;
  sent: string[] = [];
  closed: { code?: number; reason?: string } | null = null;
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: { code: number; reason: string; wasClean: boolean }) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;

  constructor(
    readonly url: string,
    readonly protocols: string[],
  ) {}

  send(data: string): void {
    if (this.readyState !== 1) throw new Error('not open');
    this.sent.push(data);
  }
  close(code?: number, reason?: string): void {
    this.closed = { code, reason };
    this.readyState = 3;
  }
  /** Server accepts the upgrade, selecting `protocol`. */
  open(protocol = this.protocols[0] ?? ''): void {
    this.protocol = protocol;
    this.readyState = 1;
    this.onopen?.({});
  }
  receive(frame: unknown): void {
    this.onmessage?.({ data: typeof frame === 'string' ? frame : JSON.stringify(frame) });
  }
  serverClose(code: number): void {
    this.readyState = 3;
    this.onclose?.({ code, reason: '', wasClean: true });
  }
  frames(): Record<string, unknown>[] {
    return this.sent.map(text => JSON.parse(text) as Record<string, unknown>);
  }
}

export class SocketHub {
  sockets: FakeSocket[] = [];
  factory = (url: string, protocols: string[]): FakeSocket => {
    const socket = new FakeSocket(url, protocols);
    this.sockets.push(socket);
    return socket;
  };
  get last(): FakeSocket {
    const socket = this.sockets[this.sockets.length - 1];
    if (!socket) throw new Error('no socket opened');
    return socket;
  }
}

export interface FakeResponseSpec {
  status: number;
  body?: unknown;
  headers?: Record<string, string>;
}

/** A fetch double: route → response spec (or a thrown error); records every call. */
export function fakeFetch(handler: (url: string, init: RequestInit) => FakeResponseSpec | Error) {
  const calls: { url: string; init: RequestInit }[] = [];
  const fetchImpl: FetchLike = async (url, init) => {
    calls.push({ url, init });
    const result = handler(url, init);
    if (result instanceof Error) throw result;
    const text = result.body === undefined ? '' : JSON.stringify(result.body);
    return new Response(result.status === 204 || result.status === 304 ? null : text, {
      status: result.status,
      headers: { 'Content-Type': 'application/json', ...result.headers },
    });
  };
  return { calls, transport: new HttpTransport(fetchImpl) };
}

/** A DaemonClient whose ticket() is scripted (no HTTP). */
export function ticketClient(tickets: (() => Promise<string>)[] = []): DaemonClient & { ticketCalls: number } {
  const client = new DaemonClient(GRANT, new HttpTransport(async () => new Response('{}', { status: 500 }))) as DaemonClient & { ticketCalls: number };
  client.ticketCalls = 0;
  client.ticket = async () => {
    const next = tickets[client.ticketCalls] ?? (() => Promise.resolve(TICKET));
    client.ticketCalls += 1;
    return next();
  };
  return client;
}

export async function flush(): Promise<void> {
  for (let i = 0; i < 5; i++) await Promise.resolve();
}
