// Startup / connect state machine (matrix startup.* rows, design §5.9).
// Framework-free so it is unit-testable; React binds it via useConnect().
//
//  idle ──start()──► needs_permission ──allowLocalAccess() (user gesture)──► probing
//    │                                                                    │
//    └──────────────(no prompt needed)──────────────────────────────────► probing ─► probed(result)
//  probed(ready_unlinked) ──connectBrowser() (user gesture)──► linking ─► (grant) ─► probing ─► connected
//  probed(*) re-probes every 3 s while on the connect screen; recheck() probes now.
import { createLink, pollLink, type LinkRequest } from '../client/links';
import { loopbackPermission, probe as defaultProbe, type LoopbackPermission, type ProbeResult } from '../client/health';
import { LinkDenied, LinkExpired, LinkGone, isAbortError } from '../client/errors';
import { currentGrant, type Grant } from '../client/grant';
import { DAEMON_HTTP } from '../client/http';
import { Subject, type Observable } from '../client/observable';
import type { BrowserHealth } from '../client/types';

export const REPROBE_INTERVAL_MS = 3_000;

export type LinkStatus = 'creating' | 'waiting' | 'denied' | 'expired' | 'error';

export type ConnectState =
  | { phase: 'idle' }
  | { phase: 'needs_permission' }
  | { phase: 'probing'; previous: ProbeResult | null }
  | { phase: 'probed'; result: ProbeResult }
  | { phase: 'linking'; health: BrowserHealth; status: LinkStatus; link: LinkRequest | null; error?: string }
  | { phase: 'connected'; grant: Grant; health: BrowserHealth };

export interface ConnectDeps {
  probe?: (signal?: AbortSignal) => Promise<ProbeResult>;
  permission?: () => Promise<LoopbackPermission>;
  createLink?: (signal?: AbortSignal) => Promise<LinkRequest>;
  pollLink?: (request: LinkRequest, signal: AbortSignal, onPending: () => void) => Promise<Grant>;
  /** A page served by the daemon's hostname needs no cross-network permission. */
  pageSharesDaemonHost?: () => boolean;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
}

function defaultPageSharesDaemonHost(): boolean {
  return typeof location !== 'undefined' && !!DAEMON_HTTP && new URL(DAEMON_HTTP).hostname === location.hostname;
}

export class ConnectController {
  private readonly subject = new Subject<ConnectState>({ phase: 'idle' });
  readonly state: Observable<ConnectState> = this.subject;
  private timer: unknown = null;
  private probeAbort: AbortController | null = null;
  private linkAbort: AbortController | null = null;
  private disposed = false;
  private readonly deps: Required<ConnectDeps>;

  constructor(deps: ConnectDeps = {}) {
    this.deps = {
      probe: deps.probe ?? (signal => defaultProbe(signal)),
      permission: deps.permission ?? (() => loopbackPermission()),
      createLink: deps.createLink ?? (signal => createLink(undefined, signal)),
      pollLink: deps.pollLink ?? ((request, signal, onPending) => pollLink(request, signal, { onPending })),
      pageSharesDaemonHost: deps.pageSharesDaemonHost ?? defaultPageSharesDaemonHost,
      setTimer: deps.setTimer ?? ((fn, ms) => setTimeout(fn, ms)),
      clearTimer: deps.clearTimer ?? (handle => clearTimeout(handle as ReturnType<typeof setTimeout>)),
    };
  }

  /** First load: probe at once unless the browser needs a user gesture for loopback access. */
  async start(): Promise<void> {
    if (this.subject.get().phase !== 'idle') return;
    const permission = await this.deps.permission();
    if (this.disposed) return;
    if (permission === 'prompt' && !this.deps.pageSharesDaemonHost()) {
      this.subject.set({ phase: 'needs_permission' });
      return;
    }
    await this.recheck();
  }

  /** 'Allow local access' button: the probe's fetch runs inside the user gesture. */
  allowLocalAccess(): Promise<void> {
    return this.recheck();
  }

  /** 'Recheck' button, the 3 s timer, and post-link verification. */
  async recheck(): Promise<void> {
    if (this.disposed) return;
    const current = this.subject.get();
    if (current.phase === 'linking' && (current.status === 'creating' || current.status === 'waiting')) return;
    this.clearTimer();
    this.probeAbort?.abort();
    const abort = new AbortController();
    this.probeAbort = abort;
    const previous = current.phase === 'probed' ? current.result : current.phase === 'probing' ? current.previous : null;
    this.subject.set({ phase: 'probing', previous });
    let result: ProbeResult;
    try {
      result = await this.deps.probe(abort.signal);
    } catch (error) {
      if (abort.signal.aborted || this.disposed) return;
      result = { kind: 'unreachable', detail: isAbortError(error) ? 'timeout' : 'network' };
    }
    if (abort.signal.aborted || this.disposed) return;
    this.probeAbort = null;
    const grant = currentGrant();
    if (result.kind === 'ready' && grant) {
      this.subject.set({ phase: 'connected', grant, health: result.health });
      return;
    }
    if (result.kind === 'ready' && !grant) result = { kind: 'ready_unlinked', health: result.health };
    this.subject.set({ phase: 'probed', result });
    this.scheduleReprobe();
  }

  /** 'Connect browser' button (user gesture): create a link and wait for owner approval. */
  async connectBrowser(): Promise<void> {
    const current = this.subject.get();
    const health =
      current.phase === 'probed' && (current.result.kind === 'ready_unlinked' || current.result.kind === 'ready')
        ? current.result.health
        : current.phase === 'linking'
          ? current.health
          : null;
    if (!health || this.disposed) return;
    this.clearTimer();
    this.linkAbort?.abort();
    const abort = new AbortController();
    this.linkAbort = abort;
    this.subject.set({ phase: 'linking', health, status: 'creating', link: null });
    let link: LinkRequest;
    try {
      link = await this.deps.createLink(abort.signal);
    } catch (error) {
      if (abort.signal.aborted) return;
      this.subject.set({ phase: 'linking', health, status: 'error', link: null, error: error instanceof Error ? error.message : 'Could not start linking.' });
      return;
    }
    if (abort.signal.aborted) return;
    this.subject.set({ phase: 'linking', health, status: 'waiting', link });
    try {
      await this.deps.pollLink(link, abort.signal, () => undefined);
    } catch (error) {
      if (abort.signal.aborted || isAbortError(error)) return;
      const status: LinkStatus = error instanceof LinkDenied ? 'denied' : error instanceof LinkExpired || error instanceof LinkGone ? 'expired' : 'error';
      this.subject.set({ phase: 'linking', health, status, link, error: error instanceof Error ? error.message : undefined });
      return;
    }
    this.linkAbort = null;
    // Leave the linking phase first: recheck() deliberately ignores calls
    // while a link is still waiting for approval.
    this.subject.set({ phase: 'probing', previous: null });
    await this.recheck();
  }

  /** Leave the link flow (or a failed link) and return to the probe result. */
  cancelLink(): void {
    this.linkAbort?.abort();
    this.linkAbort = null;
    // Leave the linking phase first (as the success path does): recheck()
    // ignores calls while a link is creating/waiting, so staying in it would
    // strand the screen with no poll and no re-probe.
    const current = this.subject.get();
    if (current.phase === 'linking') this.subject.set({ phase: 'probing', previous: { kind: 'ready_unlinked', health: current.health } });
    void this.recheck();
  }

  /** After revocation or a fatal engine loss: back to the connect screen. */
  reset(): void {
    this.linkAbort?.abort();
    this.probeAbort?.abort();
    this.clearTimer();
    this.subject.set({ phase: 'probing', previous: null });
    void this.recheck();
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimer();
    this.probeAbort?.abort();
    this.linkAbort?.abort();
  }

  private scheduleReprobe(): void {
    this.clearTimer();
    this.timer = this.deps.setTimer(() => {
      this.timer = null;
      const state = this.subject.get();
      if (state.phase === 'probed') void this.recheck();
    }, REPROBE_INTERVAL_MS);
  }

  private clearTimer(): void {
    if (this.timer !== null) this.deps.clearTimer(this.timer);
    this.timer = null;
  }
}
