// awman.terminal.v1 over a ticketed WebSocket (design §2.7, §5.4, 0141).
//
// The terminal view never opens sockets; it renders what this connection
// emits (snapshot → reset + write, output → write) and calls `input`,
// `resize`, `takeControl`. Control semantics (src/command/terminals/operations.rs):
// - `take_control` succeeds silently; a refusal is an `error` frame
//   (`control_held`, `input_not_allowed`, `conflict`) and the socket stays open;
// - input/resize are honoured only for the holder; another viewer taking
//   control is not pushed — we learn it from an `error` on our next input;
// - a reconnect releases the grant (never re-taken automatically).
// - close 1008 forwards the daemon's close reason to
//   DaemonClient.authorizationLost (SR-09): a daemon ending reason (`grant
//   revoked`, `origin removed`, `daemon stopping`…) ends with exactly that
//   state and no proof; an unexplained policy close is classified by one proof
//   and, when authorization is still live, is an ordinary reconnect (new
//   ticket, fresh snapshot, control not re-taken).
// Bounded: input chunks ≤ 64 KiB, a single paste/input ≤ 1 MiB, and nothing
// is queued while the socket is congested or closed.
import { decodeBase64, encodeBase64 } from './base64';
import { DaemonError, GrantRevoked } from './errors';
import { DAEMON_WS } from './http';
import { Emitter, Subject, type Observable } from './observable';
import {
  CLOSE_CLIENT_PROTOCOL_ERROR,
  CLOSE_NORMAL,
  CLOSE_POLICY,
  MAX_BUFFERED_BYTES,
  WS_OPEN,
  backoffDelay,
  defaultSocketFactory,
  type SocketFactory,
  type WebSocketLike,
} from './socket';
import { acceptedProtocolOk, ticketSubprotocols } from './tickets';
import {
  MAX_TERMINAL_CLIENT_MESSAGE_BYTES,
  TERMINAL_FRAME_TYPES,
  TERMINAL_INPUT_CHUNK_BYTES,
  TERMINAL_PASTE_MAX_BYTES,
  type TerminalClientFrame,
  type TerminalFrame,
} from './types';
import type { DaemonClient } from './daemonClient';

export type TerminalConnectionState = 'connecting' | 'open' | 'reconnecting' | 'closed' | 'exited';

export interface TerminalControl {
  holder: 'me' | 'other' | 'none';
  inputAllowed: boolean;
}

export interface TerminalSnapshot {
  cols: number;
  rows: number;
  screen: Uint8Array;
  hasOutput: boolean;
}

export type TerminalErrorEvent = { code: string; message: string };

export const RESIZE_DEBOUNCE_MS = 150;
export const TAKE_CONTROL_SETTLE_MS = 300;
export const MAX_TERMINAL_RECONNECTS = 20;
/** Incoming frame bound: an output frame carries at most a hub chunk; snapshots can be a full screen. */
export const MAX_TERMINAL_FRAME_CHARS = 8 * 1024 * 1024;

export interface TerminalConnectionDeps {
  socketFactory?: SocketFactory;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
}

export class TerminalConnection {
  private readonly stateSubject = new Subject<TerminalConnectionState>('connecting');
  readonly state: Observable<TerminalConnectionState> = this.stateSubject;
  private readonly controlSubject: Subject<TerminalControl>;
  readonly control: Observable<TerminalControl>;
  private readonly snapshots = new Emitter<TerminalSnapshot>();
  private readonly outputs = new Emitter<Uint8Array>();
  private readonly resizes = new Emitter<{ cols: number; rows: number }>();
  private readonly exits = new Emitter<number>();
  private readonly errors = new Emitter<TerminalErrorEvent>();
  private socket: WebSocketLike | null = null;
  private attempt = 0;
  private timer: unknown = null;
  private resizeTimer: unknown = null;
  private pendingResize: { cols: number; rows: number } | null = null;
  private controlWaiter: { resolve(): void; reject(error: Error): void; timer: unknown } | null = null;
  private done = false;
  private readonly socketFactory: SocketFactory;
  private readonly setTimer: (fn: () => void, ms: number) => unknown;
  private readonly clearTimer: (handle: unknown) => void;

  constructor(
    private readonly client: DaemonClient,
    readonly terminalId: string,
    private readonly opts: { readOnly?: boolean } = {},
    deps: TerminalConnectionDeps = {},
  ) {
    this.socketFactory = deps.socketFactory ?? defaultSocketFactory;
    this.setTimer = deps.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = deps.clearTimer ?? (handle => clearTimeout(handle as ReturnType<typeof setTimeout>));
    this.controlSubject = new Subject<TerminalControl>({ holder: 'none', inputAllowed: !opts.readOnly });
    this.control = this.controlSubject;
    void this.connect();
  }

  onSnapshot(callback: (snapshot: TerminalSnapshot) => void): () => void {
    return this.snapshots.on(callback);
  }
  onOutput(callback: (bytes: Uint8Array) => void): () => void {
    return this.outputs.on(callback);
  }
  onResize(callback: (cols: number, rows: number) => void): () => void {
    return this.resizes.on(size => callback(size.cols, size.rows));
  }
  onExit(callback: (code: number) => void): () => void {
    return this.exits.on(callback);
  }
  /** Refusals and local drops (control_held, input_backpressure, input_too_large, not_holding…). */
  onError(callback: (error: TerminalErrorEvent) => void): () => void {
    return this.errors.on(callback);
  }

  /** Send input bytes (≤ 1 MiB per call, chunked at 64 KiB). Dropped with an error otherwise. */
  input(bytes: Uint8Array): void {
    if (bytes.length === 0) return;
    if (bytes.length > TERMINAL_PASTE_MAX_BYTES) return this.errors.emit({ code: 'input_too_large', message: 'Input larger than 1 MiB was not sent.' });
    if (this.controlSubject.get().holder !== 'me') return this.errors.emit({ code: 'not_holding', message: 'Take control to type into this terminal.' });
    const socket = this.openSocket();
    if (!socket) return this.errors.emit({ code: 'not_connected', message: 'The terminal is not connected; input was not sent.' });
    if (socket.bufferedAmount + bytes.length > MAX_BUFFERED_BYTES) return this.errors.emit({ code: 'input_backpressure', message: 'The terminal is busy; input was not sent.' });
    for (let offset = 0; offset < bytes.length; offset += TERMINAL_INPUT_CHUNK_BYTES) {
      this.sendFrame(socket, { type: 'input', bytes: encodeBase64(bytes.subarray(offset, offset + TERMINAL_INPUT_CHUNK_BYTES)) });
    }
  }

  /** Debounced resize; sent only while this viewer holds control. */
  resize(cols: number, rows: number): void {
    const control = this.controlSubject.get();
    if (control.holder !== 'me' || !control.inputAllowed || !this.openSocket()) return;
    if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 1 || rows < 1 || cols > 1000 || rows > 1000) return;
    if (this.pendingResize?.cols === cols && this.pendingResize.rows === rows) return;
    this.pendingResize = { cols, rows };
    if (this.resizeTimer !== null) this.clearTimer(this.resizeTimer);
    this.resizeTimer = this.setTimer(() => {
      this.resizeTimer = null;
      const size = this.pendingResize;
      this.pendingResize = null;
      const socket = this.openSocket();
      const current = this.controlSubject.get();
      if (!size || !socket || current.holder !== 'me' || !current.inputAllowed) return;
      this.sendFrame(socket, { type: 'resize', cols: size.cols, rows: size.rows });
    }, RESIZE_DEBOUNCE_MS);
  }

  /** Invalidate measurements when hidden, unmounted, disconnected or refused. */
  cancelResize(): void {
    if (this.resizeTimer !== null) this.clearTimer(this.resizeTimer);
    this.resizeTimer = null;
    this.pendingResize = null;
  }

  /** Request the exclusive input grant; resolves when no refusal arrived within a short settle window. */
  takeControl(): Promise<void> {
    if (this.opts.readOnly) return Promise.reject(new Error('This terminal is read-only here.'));
    const socket = this.openSocket();
    if (!socket) return Promise.reject(new Error('The terminal is not connected.'));
    this.controlWaiter?.reject(new Error('superseded'));
    return new Promise<void>((resolve, reject) => {
      const timer = this.setTimer(() => {
        if (this.controlWaiter?.timer !== timer) return;
        this.controlWaiter = null;
        resolve();
      }, TAKE_CONTROL_SETTLE_MS);
      this.controlWaiter = { resolve, reject, timer };
      this.controlSubject.set({ holder: 'me', inputAllowed: true });
      this.sendFrame(socket, { type: 'take_control' });
    });
  }

  close(): void {
    if (this.done) return;
    const socket = this.openSocket();
    if (socket) this.sendFrame(socket, { type: 'detached', reason: 'user' });
    this.finish('closed');
  }

  private openSocket(): WebSocketLike | null {
    return this.socket && this.socket.readyState === WS_OPEN && this.stateSubject.get() === 'open' ? this.socket : null;
  }

  private sendFrame(socket: WebSocketLike, frame: TerminalClientFrame): void {
    const text = JSON.stringify(frame);
    if (text.length > MAX_TERMINAL_CLIENT_MESSAGE_BYTES) return this.errors.emit({ code: 'input_too_large', message: 'Frame too large.' });
    socket.send(text);
  }

  private async connect(): Promise<void> {
    if (this.done) return;
    this.attempt += 1;
    this.stateSubject.set(this.attempt === 1 ? 'connecting' : 'reconnecting');
    let ticket: string;
    try {
      ticket = await this.client.ticket('terminal', this.terminalId);
    } catch (error) {
      if (error instanceof GrantRevoked) return this.finish('closed');
      if (error instanceof DaemonError && (error.status === 404 || error.status === 410)) return this.finish('closed');
      return this.scheduleReconnect();
    }
    if (this.done) return;
    let socket: WebSocketLike;
    try {
      socket = this.socketFactory(`${DAEMON_WS}/v1/terminals/${encodeURIComponent(this.terminalId)}${this.opts.readOnly ? '?read_only=true' : ''}`, ticketSubprotocols('terminal', ticket));
    } catch {
      return this.scheduleReconnect();
    }
    this.socket = socket;
    socket.onopen = () => {
      if (socket !== this.socket) return;
      if (!acceptedProtocolOk('terminal', socket.protocol)) return this.dropSocket(socket, CLOSE_CLIENT_PROTOCOL_ERROR);
      // A new connection holds no grant until the user takes control again.
      this.controlSubject.set({ holder: 'none', inputAllowed: !this.opts.readOnly });
      this.stateSubject.set('open');
    };
    socket.onmessage = event => this.onMessage(socket, event.data);
    socket.onclose = event => this.onSocketClose(socket, event.code, event.reason);
    socket.onerror = () => undefined;
  }

  private dropSocket(socket: WebSocketLike, code: number): void {
    try {
      socket.close(code, 'protocol error');
    } catch {
      // ignore
    }
    this.onSocketClose(socket, code);
  }

  private onMessage(socket: WebSocketLike, data: unknown): void {
    if (socket !== this.socket) return;
    if (typeof data !== 'string' || data.length > MAX_TERMINAL_FRAME_CHARS) return this.dropSocket(socket, CLOSE_CLIENT_PROTOCOL_ERROR);
    let frame: TerminalFrame;
    try {
      frame = JSON.parse(data) as TerminalFrame;
    } catch {
      return this.dropSocket(socket, CLOSE_CLIENT_PROTOCOL_ERROR);
    }
    if (typeof frame !== 'object' || frame === null || !(TERMINAL_FRAME_TYPES as readonly string[]).includes(frame.type)) {
      return this.dropSocket(socket, CLOSE_CLIENT_PROTOCOL_ERROR);
    }
    try {
      this.dispatch(frame);
    } catch {
      this.dropSocket(socket, CLOSE_CLIENT_PROTOCOL_ERROR);
    }
  }

  private dispatch(frame: TerminalFrame): void {
    switch (frame.type) {
      case 'snapshot':
        this.attempt = 0;
        this.snapshots.emit({ cols: frame.cols, rows: frame.rows, screen: decodeBase64(frame.screen), hasOutput: frame.has_output ?? true });
        return;
      case 'output':
        this.outputs.emit(decodeBase64(frame.bytes));
        return;
      case 'resize':
        this.resizes.emit({ cols: frame.cols, rows: frame.rows });
        return;
      case 'lagged':
        return; // a fresh snapshot follows; the view resets on it
      case 'exit':
        this.exits.emit(frame.code);
        this.finish('exited');
        return;
      case 'error':
        this.onRefusal(frame.code, frame.error);
        return;
    }
  }

  private onRefusal(code: string, message: string): void {
    if (code === 'control_held' || code === 'input_not_allowed' || code === 'conflict') this.cancelResize();
    if (code === 'control_held') this.controlSubject.set({ holder: 'other', inputAllowed: !this.opts.readOnly });
    else if (code === 'input_not_allowed') this.controlSubject.set({ holder: 'none', inputAllowed: false });
    else if (code === 'conflict') this.controlSubject.set({ holder: 'none', inputAllowed: false });
    const waiter = this.controlWaiter;
    if (waiter && (code === 'control_held' || code === 'input_not_allowed' || code === 'conflict')) {
      this.controlWaiter = null;
      this.clearTimer(waiter.timer);
      waiter.reject(new Error(message));
    }
    this.errors.emit({ code, message });
  }

  private onSocketClose(socket: WebSocketLike, code: number, reason?: string): void {
    if (socket !== this.socket) return;
    this.socket = null;
    this.cancelResize();
    socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
    if (this.done) return;
    this.controlSubject.set({ holder: 'none', inputAllowed: !this.opts.readOnly });
    if (code === CLOSE_POLICY) {
      this.stateSubject.set('reconnecting');
      void this.client.authorizationLost(reason).then(valid => {
        if (this.done) return;
        if (!valid) return this.finish('closed');
        this.scheduleReconnect();
      });
      return;
    }
    if (code === CLOSE_NORMAL) return this.finish('closed');
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.done) return;
    if (this.attempt >= MAX_TERMINAL_RECONNECTS) return this.finish('closed');
    this.stateSubject.set('reconnecting');
    this.timer = this.setTimer(() => {
      this.timer = null;
      void this.connect();
    }, backoffDelay(this.attempt + 1));
  }

  private finish(state: 'closed' | 'exited'): void {
    if (this.done) return;
    this.done = true;
    this.cancelResize();
    for (const handle of [this.timer, this.resizeTimer, this.controlWaiter?.timer]) if (handle !== null && handle !== undefined) this.clearTimer(handle);
    this.controlWaiter?.reject(new Error('terminal closed'));
    this.controlWaiter = null;
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
      try {
        socket.close(CLOSE_NORMAL, state);
      } catch {
        // ignore
      }
    }
    this.controlSubject.set({ holder: 'none', inputAllowed: false });
    this.stateSubject.set(state);
  }
}
