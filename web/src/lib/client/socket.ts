// WebSocket seam shared by the frontend and terminal connections, so unit
// tests can drive the state machines with a fake socket.
import { RECONNECT_BACKOFF_INITIAL_MS, RECONNECT_BACKOFF_MAX_MS } from './types';

export const WS_OPEN = 1;

export interface WebSocketLike {
  readonly readyState: number;
  /** Subprotocol the server selected. */
  readonly protocol: string;
  readonly bufferedAmount: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: ((event: { code: number; reason: string; wasClean: boolean }) => void) | null;
  onerror: ((event: unknown) => void) | null;
}

export type SocketFactory = (url: string, protocols: string[]) => WebSocketLike;

export const defaultSocketFactory: SocketFactory = (url, protocols) => new WebSocket(url, protocols) as unknown as WebSocketLike;

/** Client-initiated close codes (browsers only allow 1000 and 3000–4999). */
export const CLOSE_NORMAL = 1000;
export const CLOSE_CLIENT_PROTOCOL_ERROR = 4002;
export const CLOSE_CLIENT_TOO_BIG = 4009;
/** Server close codes the client reacts to (design §2.8). */
export const CLOSE_POLICY = 1008;
export const CLOSE_GOING_AWAY = 1001;

/** Refuse to queue more than this many unsent bytes on a socket (bounded queues). */
export const MAX_BUFFERED_BYTES = 4 * 1024 * 1024;

/** Exponential reconnect backoff, 50 ms → 1 s (frontend_protocol.rs constants). */
export function backoffDelay(attempt: number): number {
  return Math.min(RECONNECT_BACKOFF_MAX_MS, RECONNECT_BACKOFF_INITIAL_MS * 2 ** Math.max(0, attempt - 1));
}

export function utf8Length(text: string): number {
  return new TextEncoder().encode(text).length;
}
