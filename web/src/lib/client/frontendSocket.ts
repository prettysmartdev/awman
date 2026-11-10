// awman.frontend.v1 over a ticketed WebSocket (design §2.7, §3.5, §5.4).
//
// Rules enforced here (unit-tested in test/lib/frontendSocket.test.ts):
// - Outbound `seq` starts at 1 per connection. Inbound `seq` must be
//   contiguous from 1, the first frame must be `welcome`, and `type` must be
//   known; anything else is a protocol error → close and reconnect with a
//   full resync from the next Snapshot.
// - Every frame is runtime-validated (validate.ts) before it touches the
//   store: unknown delta kinds, malformed values/snapshots, a repeated or
//   mismatched Welcome (protocol, instance id, requested role) and any reducer
//   failure are protocol errors (close → new ticket → full Snapshot), never a
//   corrupted authoritative snapshot.
// - SnapshotChunk reassembly is bounded (16 MiB) and strictly ordered.
// - Reconnect: backoff 50 ms → 1 s, a NEW ticket every time, `Hello{resume}`
//   for a primary within WEB_PRIMARY_RECONNECT_GRACE; after the grace the
//   instance is marked ended.
// - No replay: a Submit / DialogResponse / UiRequest whose outcome was not
//   observed before the socket dropped is never re-sent; it is surfaced as a
//   "not confirmed — check the log" notice and the snapshot is the truth.
// - `send` refuses (throws) while not open, and when the socket's unsent
//   buffer exceeds MAX_BUFFERED_BYTES: nothing is queued for later.
import { decodeBase64 } from './base64';
import { DaemonError, GrantRevoked } from './errors';
import { DAEMON_WS } from './http';
import { InstanceStore } from './instanceStore';
import { Emitter, Subject, type Observable } from './observable';
import {
  CLOSE_CLIENT_PROTOCOL_ERROR,
  CLOSE_CLIENT_TOO_BIG,
  CLOSE_NORMAL,
  CLOSE_POLICY,
  MAX_BUFFERED_BYTES,
  WS_OPEN,
  backoffDelay,
  defaultSocketFactory,
  utf8Length,
  type SocketFactory,
  type WebSocketLike,
} from './socket';
import { acceptedProtocolOk, ticketSubprotocols } from './tickets';
import { isEngineFrame, isInstanceSnapshot } from './validate';
import {
  CLIENT_MAX_SNAPSHOT_BYTES,
  ENGINE_FRAME_TYPES,
  FRONTEND_PROTOCOL,
  MAX_FRAME_BYTES,
  WEB_PRIMARY_RECONNECT_GRACE_MS,
  type EngineFrame,
  type FrontendFrame,
  type FrontendFrameOf,
  type FrontendRole,
  type Hello,
  type InstanceDelta,
  type InstanceSnapshot,
  type Seq,
  type TermSize,
  type UiOutcome,
  type UiRequest,
  type ViewScope,
} from './types';
import type { DaemonClient } from './daemonClient';

export const WEB_BUILD = 'awman-web/0.1.0';

export type ConnectionState = 'connecting' | 'open' | 'reconnecting' | 'closed';

export type CloseReason =
  | { kind: 'user' }
  | { kind: 'navigation' }
  | { kind: 'revoked' }
  | { kind: 'refused'; code: string; error: string }
  | { kind: 'detached'; reason: string }
  | { kind: 'grace_expired' }
  | { kind: 'not_found' };

export interface HelloOptions {
  role: FrontendRole;
  /** Required for a new web primary (design §3.5). */
  launchContext?: string;
  /** Secondary only. */
  view?: ViewScope;
  termSize: TermSize;
  /** Resume a previous frontend id (sessionStorage after a reload, §5.7). */
  resumeFrontendId?: string;
}

export type OutboundFrame = Exclude<FrontendFrame, { type: 'hello' }>;

export interface FrontendConnectionDeps {
  socketFactory?: SocketFactory;
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
}

export class NotConnectedError extends Error {
  override readonly name = 'NotConnectedError';
  constructor() {
    super('Not connected to the awman engine; the action was not sent.');
  }
}

export class BackpressureError extends Error {
  override readonly name = 'BackpressureError';
  constructor() {
    super('The connection is congested; the action was not sent.');
  }
}

interface PendingUi {
  resolve(outcome: UiOutcome): void;
  reject(error: Error): void;
}

export class FrontendConnection {
  readonly store = new InstanceStore();
  private readonly stateSubject = new Subject<ConnectionState>('connecting');
  readonly state: Observable<ConnectionState> = this.stateSubject;
  private readonly roleSubject = new Subject<FrontendRole | null>(null);
  /** Role the engine assigned in Welcome. */
  readonly role: Observable<FrontendRole | null> = this.roleSubject;
  private readonly frames = new Emitter<EngineFrame>();
  private readonly closeEmitter = new Emitter<CloseReason>();

  private socket: WebSocketLike | null = null;
  private frontendId: string | null;
  private outSeq = 0;
  private inSeq = 0;
  private chunks: { of: number; parts: Uint8Array[]; bytes: number } | null = null;
  private attempt = 0;
  private disconnectedAt: number | null = null;
  private timer: unknown = null;
  private closedReason: CloseReason | null = null;
  private refusal: { code: string; error: string } | null = null;
  private readonly pendingSubmits = new Map<string, string>(); // tab_id → description
  private readonly pendingAnswers = new Set<string>(); // dialog_id
  private readonly pendingUi = new Map<Seq, PendingUi>();
  private readonly socketFactory: SocketFactory;
  private readonly now: () => number;
  private readonly setTimer: (fn: () => void, ms: number) => unknown;
  private readonly clearTimer: (handle: unknown) => void;

  constructor(
    private readonly client: DaemonClient,
    readonly instanceId: string,
    private readonly hello: HelloOptions,
    deps: FrontendConnectionDeps = {},
  ) {
    this.socketFactory = deps.socketFactory ?? defaultSocketFactory;
    this.now = deps.now ?? Date.now;
    this.setTimer = deps.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = deps.clearTimer ?? (handle => clearTimeout(handle as ReturnType<typeof setTimeout>));
    this.frontendId = hello.resumeFrontendId ?? null;
    void this.connect();
  }

  /** Frontend id assigned by the engine (for sessionStorage resume). */
  get id(): string | null {
    return this.frontendId;
  }

  get closeReason(): CloseReason | null {
    return this.closedReason;
  }

  onFrame(callback: (frame: EngineFrame) => void): () => void {
    return this.frames.on(callback);
  }

  onClose(callback: (reason: CloseReason) => void): () => void {
    return this.closeEmitter.on(callback);
  }

  /** Send one frame; returns its seq. Throws when not open or congested (never queued). */
  send(frame: OutboundFrame): Seq {
    const socket = this.socket;
    if (this.stateSubject.get() !== 'open' || !socket || socket.readyState !== WS_OPEN) throw new NotConnectedError();
    if (socket.bufferedAmount > MAX_BUFFERED_BYTES) throw new BackpressureError();
    const seq = this.outSeq + 1;
    const text = JSON.stringify({ seq, ...frame });
    if (utf8Length(text) > MAX_FRAME_BYTES) throw new RangeError('frame exceeds the 1 MiB protocol limit');
    socket.send(text);
    this.outSeq = seq;
    this.track(frame);
    return seq;
  }

  /** UiRequest with its UiAck outcome (the only seq-correlated ack). */
  uiRequest(request: UiRequest): Promise<UiOutcome> {
    return new Promise((resolve, reject) => {
      const seq = this.send({ type: 'ui_request', ...request } as FrontendFrameOf<'ui_request'>);
      this.pendingUi.set(seq, { resolve, reject });
    });
  }

  close(reason: 'user' | 'navigation' = 'user'): void {
    this.finish({ kind: reason });
  }

  private track(frame: OutboundFrame): void {
    if (frame.type === 'submit') {
      const what = frame.input.source === 'command_box' ? frame.input.raw : frame.input.args.join(' ');
      this.pendingSubmits.set(frame.tab_id, what);
    } else if (frame.type === 'dialog_response') {
      this.pendingAnswers.add(frame.dialog_id);
    }
  }

  private async connect(): Promise<void> {
    if (this.closedReason) return;
    this.attempt += 1;
    this.stateSubject.set(this.attempt === 1 && this.disconnectedAt === null ? 'connecting' : 'reconnecting');
    let ticket: string;
    try {
      ticket = await this.client.ticket('frontend', this.instanceId);
    } catch (error) {
      if (error instanceof GrantRevoked) return this.finish({ kind: 'revoked' });
      if (error instanceof DaemonError && (error.status === 404 || error.code === 'not_found')) return this.finish({ kind: 'not_found' });
      return this.scheduleReconnect();
    }
    if (this.closedReason) return;
    const url = `${DAEMON_WS}/v1/engine/instances/${encodeURIComponent(this.instanceId)}/frontend`;
    let socket: WebSocketLike;
    try {
      socket = this.socketFactory(url, ticketSubprotocols('frontend', ticket));
    } catch {
      return this.scheduleReconnect();
    }
    this.socket = socket;
    this.outSeq = 0;
    this.inSeq = 0;
    this.chunks = null;
    this.refusal = null;
    socket.onopen = () => this.onOpen(socket);
    socket.onmessage = event => this.onMessage(socket, event.data);
    socket.onclose = event => this.onSocketClose(socket, event.code);
    socket.onerror = () => undefined; // onclose follows
  }

  private onOpen(socket: WebSocketLike): void {
    if (socket !== this.socket) return;
    if (!acceptedProtocolOk('frontend', socket.protocol)) {
      this.protocolError(socket, 'server selected an unexpected subprotocol');
      return;
    }
    const resume = this.resumeId();
    const hello: Hello = {
      protocol: FRONTEND_PROTOCOL,
      build: WEB_BUILD,
      kind: 'web',
      role: this.hello.role,
      annotations: {},
      cwd: '',
      argv: [],
      env: {},
      term_size: this.hello.termSize,
      stdin_is_tty: false,
      stdout_is_tty: false,
      ...(this.hello.role === 'secondary' ? { view: this.hello.view ?? 'all' } : {}),
      ...(resume ? { resume } : {}),
      ...(!resume && this.hello.role === 'primary' && this.hello.launchContext ? { launch_context: this.hello.launchContext } : {}),
    };
    this.outSeq = 1;
    socket.send(JSON.stringify({ seq: 1, type: 'hello', ...hello }));
  }

  /** Resume only a primary, and only within the reconnect grace. */
  private resumeId(): string | null {
    if (this.hello.role !== 'primary' || !this.frontendId) return null;
    if (this.disconnectedAt === null) return this.frontendId; // reload resume from sessionStorage
    return this.now() - this.disconnectedAt < WEB_PRIMARY_RECONNECT_GRACE_MS ? this.frontendId : null;
  }

  private protocolError(socket: WebSocketLike, _why: string, code = CLOSE_CLIENT_PROTOCOL_ERROR): void {
    // Close and let onSocketClose drive the reconnect + full resync.
    try {
      socket.close(code, 'protocol error');
    } catch {
      // already closing
    }
    if (socket === this.socket) this.onSocketClose(socket, code);
  }

  private onMessage(socket: WebSocketLike, data: unknown): void {
    if (socket !== this.socket) return;
    if (typeof data !== 'string') return this.protocolError(socket, 'binary frame');
    if (data.length > MAX_FRAME_BYTES || utf8Length(data) > MAX_FRAME_BYTES) return this.protocolError(socket, 'oversized frame', CLOSE_CLIENT_TOO_BIG);
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(data) as Record<string, unknown>;
    } catch {
      return this.protocolError(socket, 'invalid JSON');
    }
    if (typeof parsed !== 'object' || parsed === null) return this.protocolError(socket, 'invalid frame');
    if (!('seq' in parsed) && typeof parsed.code === 'string') {
      // Bare refusal (attach refused / invalid hello); the socket closes next.
      this.refusal = { code: parsed.code, error: typeof parsed.error === 'string' ? parsed.error : parsed.code };
      return;
    }
    const seq = parsed.seq;
    if (typeof seq !== 'number' || seq !== this.inSeq + 1) return this.protocolError(socket, 'sequence gap');
    if (typeof parsed.type !== 'string' || !(ENGINE_FRAME_TYPES as readonly string[]).includes(parsed.type)) return this.protocolError(socket, 'unknown frame type');
    if ((seq === 1) !== (parsed.type === 'welcome')) return this.protocolError(socket, 'welcome must be exactly the first frame');
    if (!isEngineFrame(parsed)) return this.protocolError(socket, 'malformed frame');
    const frame = parsed as unknown as EngineFrame;
    if (frame.type === 'welcome' && !this.welcomeOk(frame)) return this.protocolError(socket, 'welcome does not match this hello');
    this.inSeq = seq;
    let alive: boolean;
    try {
      alive = this.dispatch(socket, frame);
    } catch {
      // A frame the validator admitted but the reducers could not apply:
      // never keep a half-applied view; resync from a fresh snapshot.
      return this.protocolError(socket, 'frame could not be applied');
    }
    if (!alive) return;
    this.frames.emit(frame);
  }

  private welcomeOk(frame: Extract<EngineFrame, { type: 'welcome' }>): boolean {
    return frame.protocol === FRONTEND_PROTOCOL && frame.instance_id === this.instanceId && frame.role === this.hello.role;
  }

  /** Returns false when the frame aborted the connection. */
  private dispatch(socket: WebSocketLike, frame: EngineFrame): boolean {
    switch (frame.type) {
      case 'welcome':
        this.frontendId = frame.frontend_id;
        this.roleSubject.set(frame.role);
        return true;
      case 'snapshot': {
        const { type: _type, seq: _seq, ...snapshot } = frame as EngineFrame & { seq: number };
        this.acceptSnapshot(snapshot as unknown as InstanceSnapshot);
        return true;
      }
      case 'snapshot_chunk':
        return this.acceptChunk(socket, frame);
      case 'delta': {
        const { type: _type, seq: _seq, ...delta } = frame as EngineFrame & { seq: number };
        const typed = delta as unknown as InstanceDelta;
        this.store.applyDelta(typed);
        if (typed.delta === 'phase') this.pendingSubmits.delete(typed.value.tab_id);
        return true;
      }
      case 'command_bytes':
        try {
          this.store.appendBytes(frame.tab_id, frame.stream, decodeBase64(frame.bytes));
        } catch {
          this.protocolError(socket, 'invalid base64');
          return false;
        }
        return true;
      case 'command_finished':
        this.pendingSubmits.delete(frame.tab_id);
        this.store.applyFrame(frame);
        return true;
      case 'dialog_closed':
        this.pendingAnswers.delete(frame.dialog_id);
        this.store.applyFrame(frame);
        return true;
      case 'ui_ack': {
        const pending = this.pendingUi.get(frame.request_seq);
        this.pendingUi.delete(frame.request_seq);
        pending?.resolve(frame.outcome);
        return true;
      }
      case 'detach':
        this.store.applyFrame(frame);
        this.finish({ kind: 'detached', reason: frame.reason });
        return false;
      default:
        this.store.applyFrame(frame);
        return true;
    }
  }

  private acceptSnapshot(snapshot: InstanceSnapshot): void {
    if (snapshot.instance_id !== this.instanceId) throw new Error('snapshot for another instance');
    this.store.applySnapshot(snapshot);
    this.attempt = 0;
    this.disconnectedAt = null;
    this.stateSubject.set('open');
  }

  private acceptChunk(socket: WebSocketLike, frame: Extract<EngineFrame, { type: 'snapshot_chunk' }>): boolean {
    const expected = this.chunks?.parts.length ?? 0;
    if (frame.chunk !== expected || frame.of < 1 || (this.chunks && this.chunks.of !== frame.of) || frame.chunk >= frame.of) {
      this.protocolError(socket, 'snapshot chunk out of order');
      return false;
    }
    let bytes: Uint8Array;
    try {
      bytes = decodeBase64(frame.bytes);
    } catch {
      this.protocolError(socket, 'invalid base64');
      return false;
    }
    const chunks = this.chunks ?? { of: frame.of, parts: [], bytes: 0 };
    chunks.parts.push(bytes);
    chunks.bytes += bytes.length;
    if (chunks.bytes > CLIENT_MAX_SNAPSHOT_BYTES) {
      this.protocolError(socket, 'snapshot too large', CLOSE_CLIENT_TOO_BIG);
      return false;
    }
    this.chunks = chunks;
    if (chunks.parts.length < chunks.of) return true;
    this.chunks = null;
    const joined = new Uint8Array(chunks.bytes);
    let offset = 0;
    for (const part of chunks.parts) {
      joined.set(part, offset);
      offset += part.length;
    }
    let snapshot: unknown;
    try {
      snapshot = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(joined));
    } catch {
      snapshot = null;
    }
    if (!isInstanceSnapshot(snapshot)) {
      this.protocolError(socket, 'invalid chunked snapshot');
      return false;
    }
    this.acceptSnapshot(snapshot);
    return true;
  }

  private onSocketClose(socket: WebSocketLike, code: number): void {
    if (socket !== this.socket) return;
    this.socket = null;
    socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
    if (this.closedReason) return;
    if (code === CLOSE_POLICY) {
      this.client.markRevoked();
      return this.finish({ kind: 'revoked' });
    }
    // Every refusal is final: never loop on the daemon's "no".
    if (this.refusal) return this.finish({ kind: 'refused', ...this.refusal });
    this.surfaceUnconfirmed();
    if (this.disconnectedAt === null) this.disconnectedAt = this.now();
    this.store.beginResync();
    this.scheduleReconnect();
  }

  /** Never replay: report what we cannot confirm and forget it. */
  private surfaceUnconfirmed(): void {
    for (const [tabId, what] of this.pendingSubmits) {
      this.store.notice('warning', `Not confirmed — check the log: "${what.length > 80 ? `${what.slice(0, 77)}…` : what}" may or may not have run.`, 'client', tabId);
    }
    if (this.pendingAnswers.size) {
      this.store.notice('warning', 'A dialog answer was not confirmed — check the log; the dialog is shown again if it is still open.', 'client');
    }
    this.pendingSubmits.clear();
    this.pendingAnswers.clear();
    for (const pending of this.pendingUi.values()) pending.reject(new NotConnectedError());
    this.pendingUi.clear();
  }

  private scheduleReconnect(): void {
    if (this.closedReason) return;
    if (this.disconnectedAt === null) this.disconnectedAt = this.now();
    // A primary may resume only within the web grace; a secondary retries
    // for the same bounded window, then gives up instead of looping forever.
    if (this.now() - this.disconnectedAt >= WEB_PRIMARY_RECONNECT_GRACE_MS) {
      return this.finish({ kind: 'grace_expired' });
    }
    this.stateSubject.set('reconnecting');
    const delay = backoffDelay(this.attempt + 1);
    this.timer = this.setTimer(() => {
      this.timer = null;
      void this.connect();
    }, delay);
  }

  private finish(reason: CloseReason): void {
    if (this.closedReason) return;
    this.closedReason = reason;
    if (this.timer !== null) this.clearTimer(this.timer);
    this.timer = null;
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
      try {
        socket.close(CLOSE_NORMAL, reason.kind);
      } catch {
        // ignore
      }
    }
    for (const pending of this.pendingUi.values()) pending.reject(new NotConnectedError());
    this.pendingUi.clear();
    if (reason.kind === 'grace_expired') this.store.markEnded('connection lost for longer than the reconnect grace');
    if (reason.kind === 'not_found') this.store.markEnded('instance not found (the daemon may have restarted)');
    if (reason.kind === 'revoked') this.store.markEnded('browser access was revoked');
    if (reason.kind === 'refused') this.store.markEnded(`${reason.code}: ${reason.error}`);
    this.stateSubject.set('closed');
    this.closeEmitter.emit(reason);
    this.frames.clear();
  }
}
