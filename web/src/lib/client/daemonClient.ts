// Grant-authenticated HTTP client for the daemon on the loaded page's host.
// Holds the grant in memory only.
//
// Authorization loss is classified from evidence (WI 0160 C3), never assumed:
// - a 401 browser_grant_invalid/required runs ONE saved-key proof. If it
//   returns the same bearer the request is retried once; refused again →
//   `authorization_unavailable`, with no further cycle.
// - a 1008 close with a daemon reason (`grant revoked`, `grant expired`,
//   `origin removed`, `listener closed`, `daemon stopping`) ends at once with
//   that reason; any other 1008 is classified by one proof.
// - a proof answered 429 is temporary (SR-08), never an ending: the client
//   waits the daemon's Retry-After (clamped to 1–120 s) and proves again, up
//   to MAX_THROTTLED_PROOFS times. The credential is neither suppressed nor
//   deleted. Sockets (and their ticket requests) wait for the verdict and then
//   reconnect normally; any other request that met the 401 fails with a
//   `rate_limited` DaemonError and is NOT resent later (no action replay).
//   Only exhausting the bound ends, as `daemon_unreachable` (key kept, not
//   suppressed), handing over to the connect screen's own Retry-After loop.
// The ending fires `onAuthEnded(state)` (and `onRevoked`) once.
import { credentialStore } from './credentialStore';
import { DaemonError, GrantRevoked } from './errors';
import { endAuthorization, isGrantLive, type Grant } from './grant';
import { recoverGrant, suppressRecovery, type RecoveryOutcome } from './reauth';
import { RECOVERY_COPY, endDiscardsCredential, endStateForCloseReason, type AuthEndState } from './recovery';
import { clearResume, setResumeBinding } from '../state/session';
import { HttpTransport, type RequestOptions } from './http';
import { completions, type Completion } from './catalogue';
import { Emitter } from './observable';
import { TICKET_RE, type TicketRoute } from './tickets';
import type {
  Annotations,
  BrowserTicketRequest,
  BrowserTicketResponse,
  Catalogue,
  CreateInstanceResponse,
  InstanceSummary,
  LaunchContextList,
  List,
  PromptKind,
  PromptView,
  SquadRun,
  SquadRunCanceled,
  SquadRunCreated,
  SquadTask,
  SquadTaskPatch,
  StatusResponse,
  TerminalRecord,
} from './types';

const REVOKED_CODES = new Set(['browser_grant_invalid', 'browser_grant_required']);

/**
 * An unexplained policy close waits this long before its proof, so a sibling
 * socket closed for the same cause with an explicit reason (e.g. `daemon
 * stopping`) classifies the ending first.
 */
export const UNEXPLAINED_CLOSE_DELAY_MS = 250;

/** Bound on one Retry-After wait while proof is throttled. */
export const MAX_THROTTLE_WAIT_SEC = 120;
/** Throttled proofs before the in-document wait gives up (non-destructively). */
export const MAX_THROTTLED_PROOFS = 5;
const TICKET_PATH = '/v1/browser-tickets';

export interface DaemonClientDeps {
  /** Prove the saved key of this grant once (default: credential store + reauth). */
  recover?: (grant: Grant) => Promise<RecoveryOutcome>;
  /** Forget a saved key after a verified ending (default: credential store). */
  forgetCredential?: (credentialId: string) => Promise<void>;
  /** Delay before classifying an unexplained 1008 (default UNEXPLAINED_CLOSE_DELAY_MS). */
  unexplainedCloseDelayMs?: number;
  /** Waits out a throttled proof's Retry-After (default setTimeout). */
  sleep?: (ms: number) => Promise<void>;
}

export class DaemonClient {
  private revoked = false;
  private ending: AuthEndState | null = null;
  private classifying: Promise<boolean> | null = null;
  private readonly revokedEmitter = new Emitter<void>();
  private readonly endedEmitter = new Emitter<AuthEndState>();
  private catalogueCache: Catalogue | null = null;
  private readonly recover: (grant: Grant) => Promise<RecoveryOutcome>;
  private readonly forgetCredential: (credentialId: string) => Promise<void>;
  private readonly unexplainedDelayMs: number;
  private readonly sleep: (ms: number) => Promise<void>;
  /** Set while the running classification waits out a 429 (epoch ms of the next proof). */
  private throttledUntil: number | null = null;
  private readonly throttledEmitter = new Emitter<number>();

  constructor(
    readonly grant: Grant,
    readonly transport: HttpTransport = new HttpTransport(),
    deps: DaemonClientDeps = {},
  ) {
    this.recover = deps.recover ?? (value => recoverGrant(value, { transport: this.transport }));
    this.forgetCredential = deps.forgetCredential ?? (id => credentialStore().remove(id));
    this.unexplainedDelayMs = deps.unexplainedCloseDelayMs ?? UNEXPLAINED_CLOSE_DELAY_MS;
    this.sleep = deps.sleep ?? (ms => new Promise(resolve => setTimeout(resolve, ms)));
  }

  /**
   * Fires with the seconds until the next automatic proof each time a
   * classification proof is throttled (429). Not an ending: present it as the
   * temporary `rate_limited` state.
   */
  onRecoveryThrottled(callback: (retryAfterSec: number) => void): () => void {
    return this.throttledEmitter.on(callback);
  }

  /** Epoch ms of the next automatic proof while throttled, else null. */
  get recoveryThrottledUntil(): number | null {
    return this.throttledUntil;
  }

  /** Why authorization ended, once it has. */
  get endState(): AuthEndState | null {
    return this.ending;
  }

  /** Fires once with the classified C7 state when this authorization ends. */
  onAuthEnded(callback: (state: AuthEndState) => void): () => void {
    if (this.ending) {
      const state = this.ending;
      queueMicrotask(() => callback(state));
      return () => undefined;
    }
    return this.endedEmitter.on(callback);
  }

  get isRevoked(): boolean {
    return this.revoked || !isGrantLive(this.grant);
  }

  /** Fires once when authorization ends for any classified reason (see onAuthEnded). */
  onRevoked(callback: () => void): () => void {
    if (this.revoked) {
      queueMicrotask(callback);
      return () => undefined;
    }
    return this.revokedEmitter.on(callback);
  }

  /**
   * Called by sockets on close 1008 with the close reason; idempotent. A
   * daemon ending reason ends at once; anything else is classified.
   */
  markRevoked(reason?: string): void {
    void this.authorizationLost(reason);
  }

  /**
   * A socket was refused with 1008. Resolves true when the authorization is
   * still valid (an unexplained policy close disproven by one proof), false
   * when it ended.
   */
  authorizationLost(reason?: string): Promise<boolean> {
    if (this.revoked) return Promise.resolve(false);
    const state = endStateForCloseReason(reason);
    if (state) {
      this.end(state);
      return Promise.resolve(false);
    }
    return new Promise<void>(resolve => setTimeout(resolve, this.unexplainedDelayMs)).then(() => this.classify(false));
  }

  /** End this authorization with a classified reason; idempotent (first reason wins). */
  end(state: AuthEndState): void {
    if (this.revoked) return;
    this.revoked = true;
    this.ending = state;
    this.catalogueCache = null;
    const credentialId = this.grant.reauth?.credentialId;
    if (credentialId && endDiscardsCredential(state)) void this.forgetCredential(credentialId);
    if (credentialId && state === 'authorization_unavailable') suppressRecovery(credentialId);
    if (endDiscardsCredential(state)) clearResume();
    setResumeBinding(null);
    endAuthorization(state);
    this.endedEmitter.emit(state);
    this.endedEmitter.clear();
    this.revokedEmitter.emit();
    this.revokedEmitter.clear();
  }

  /** One proof to classify a lost bearer; concurrent losses share it. */
  private classify(afterRecovery: boolean): Promise<boolean> {
    if (this.revoked) return Promise.resolve(false);
    if (afterRecovery) {
      // The proof returned the same bearer and it was refused again: stop.
      this.end('authorization_unavailable');
      return Promise.resolve(false);
    }
    this.classifying ??= (async () => {
      let outcome: RecoveryOutcome;
      for (let proofs = 1; ; proofs++) {
        try {
          outcome = await this.recover(this.grant);
        } catch {
          outcome = { kind: 'invalid' };
        }
        // Another socket's explicit reason may have ended us meanwhile: first reason wins.
        if (this.revoked) return false;
        if (outcome.kind !== 'rate_limited') break;
        if (proofs >= MAX_THROTTLED_PROOFS) {
          this.end('daemon_unreachable');
          return false;
        }
        const waitSec = Math.min(MAX_THROTTLE_WAIT_SEC, Math.max(1, Math.ceil(outcome.retryAfterSec)));
        this.throttledUntil = Date.now() + waitSec * 1000;
        this.throttledEmitter.emit(waitSec);
        await this.sleep(waitSec * 1000);
        this.throttledUntil = null;
        if (this.revoked) return false;
      }
      if (outcome.kind === 'granted' && outcome.grant.token === this.grant.token && outcome.grant.id === this.grant.id) return true;
      if (outcome.kind === 'ended') this.end(outcome.state);
      else if (outcome.kind === 'unreachable') this.end('daemon_unreachable');
      else if (outcome.kind === 'no_credential') this.end(isGrantLive(this.grant) ? 'authorization_unavailable' : 'authorization_expired');
      else this.end('authorization_unavailable');
      return false;
    })().finally(() => {
      this.classifying = null;
      this.throttledUntil = null;
    });
    return this.classifying;
  }

  /** The classification verdict, or its first throttle (without waiting it out). */
  private classifyUntilThrottled(): Promise<boolean | { retryAfterSec: number }> {
    const classification = this.classify(false);
    return new Promise(resolve => {
      const off = this.throttledEmitter.on(retryAfterSec => {
        off();
        resolve({ retryAfterSec });
      });
      if (this.throttledUntil !== null) {
        off();
        resolve({ retryAfterSec: Math.max(1, Math.ceil((this.throttledUntil - Date.now()) / 1000)) });
      }
      void classification.then(verdict => {
        off();
        resolve(verdict);
      });
    });
  }

  private async request<T>(path: string, options: RequestOptions = {}, recovered = false): Promise<{ status: number; body: T; headers: Headers }> {
    if (this.isRevoked) throw new GrantRevoked();
    try {
      return await this.transport.json<T>(path, { ...options, bearer: this.grant.token });
    } catch (error) {
      if (error instanceof DaemonError && error.status === 401 && REVOKED_CODES.has(String(error.code))) {
        if (recovered) {
          await this.classify(true);
          throw new GrantRevoked();
        }
        // Socket ticket requests wait out a throttled proof (the socket then
        // resynchronizes from a snapshot); every other request stops at the
        // first 429 and is never sent again by the client.
        const verdict = path === TICKET_PATH ? await this.classify(false) : await this.classifyUntilThrottled();
        // The daemon refused before acting, so one immediate retry after a proof is safe.
        if (verdict === true) return this.request<T>(path, options, true);
        if (verdict === false) throw new GrantRevoked();
        throw new DaemonError(429, 'rate_limited', RECOVERY_COPY.rate_limited, verdict.retryAfterSec);
      }
      throw error;
    }
  }

  async status(): Promise<StatusResponse> {
    return (await this.request<StatusResponse>('/v1/status')).body;
  }

  async launchContexts(): Promise<LaunchContextList> {
    return (await this.request<LaunchContextList>('/v1/engine/launch_contexts')).body;
  }

  /** GET /v1/engine/catalogue with an in-memory ETag cache (If-None-Match → 304). */
  async catalogue(): Promise<Catalogue> {
    const cached = this.catalogueCache;
    const response = await this.request<Catalogue | null>('/v1/engine/catalogue', cached ? { headers: { 'If-None-Match': `"${cached.etag}"` } } : {});
    if (response.status === 304 && cached) return cached;
    if (!response.body) throw new DaemonError(response.status, 'invalid_response', 'empty catalogue');
    this.catalogueCache = response.body;
    return response.body;
  }

  /** TUI command-box completion over the cached catalogue (port of tui_completions). */
  async complete(partial: string): Promise<Completion[]> {
    return completions(await this.catalogue(), partial);
  }

  /** Layer 2 prompt wording (design §2.9, D8). */
  async prompt(kind: PromptKind, params: Record<string, string> = {}): Promise<PromptView> {
    const query = new URLSearchParams({ kind, ...params });
    return (await this.request<PromptView>(`/v1/engine/prompts?${query.toString()}`)).body;
  }

  async createInstance(annotations: Annotations = {}): Promise<CreateInstanceResponse> {
    return (await this.request<CreateInstanceResponse>('/v1/engine/instances', { method: 'POST', body: { annotations } })).body;
  }

  /** Instances this grant may see (web primaries only, design §2.3 D3). */
  async listInstances(): Promise<InstanceSummary[]> {
    const { body } = await this.request<List<InstanceSummary>>('/v1/engine/instances');
    return body.items.filter(item => item.primary_frontend_kind === 'web' || item.primary_frontend_kind === null);
  }

  /** One-use WebSocket upgrade ticket (design §2.7). */
  async ticket(route: TicketRoute, resourceId: string): Promise<string> {
    const body: BrowserTicketRequest = route === 'frontend' ? { route, instance_id: resourceId } : { route, terminal_id: resourceId };
    const response = await this.request<BrowserTicketResponse>('/v1/browser-tickets', { method: 'POST', body });
    if (!TICKET_RE.test(response.body.ticket)) throw new DaemonError(response.status, 'invalid_response', 'malformed ticket');
    return response.body.ticket;
  }

  async terminals(ownerId: string): Promise<TerminalRecord[]> {
    const query = new URLSearchParams({ owner_id: ownerId });
    return (await this.request<List<TerminalRecord>>(`/v1/terminals?${query.toString()}`)).body.items;
  }

  async terminal(id: string): Promise<TerminalRecord> {
    return (await this.request<TerminalRecord>(`/v1/terminals/${encodeURIComponent(id)}`)).body;
  }

  /** `/v1/squad/*` on the same browser listener (design D11). */
  readonly squad = {
    tasks: async (): Promise<SquadTask[]> => (await this.request<List<SquadTask>>('/v1/squad/tasks')).body.items,
    task: async (id: string): Promise<SquadTask> => (await this.request<SquadTask>(`/v1/squad/tasks/${encodeURIComponent(id)}`)).body,
    runs: async (id: string): Promise<SquadRun[]> => (await this.request<List<SquadRun>>(`/v1/squad/tasks/${encodeURIComponent(id)}/runs`)).body.items,
    patch: async (id: string, patch: SquadTaskPatch): Promise<SquadTask> =>
      (await this.request<SquadTask>(`/v1/squad/tasks/${encodeURIComponent(id)}`, { method: 'PATCH', body: patch })).body,
    trigger: async (id: string): Promise<SquadRunCreated> =>
      (await this.request<SquadRunCreated>(`/v1/squad/tasks/${encodeURIComponent(id)}/runs`, { method: 'POST', body: {} })).body,
    cancelRun: async (runId: string): Promise<SquadRunCanceled> =>
      (await this.request<SquadRunCanceled>(`/v1/squad/runs/${encodeURIComponent(runId)}/cancel`, { method: 'POST', body: {} })).body,
    delete: async (id: string): Promise<void> => {
      await this.request<null>(`/v1/squad/tasks/${encodeURIComponent(id)}`, { method: 'DELETE' });
    },
  };
}
