// Startup / connect state machine (matrix startup.* rows, design §5.9, WI 0160 C3–C7).
// Framework-free so it is unit-testable; React binds it via useConnect().
//
//  idle ──start()──► needs_permission ──allowLocalAccess() (user gesture)──► probing
//    │                                                                    │
//    └──────────────(no prompt needed)──────────────────────────────────► probing ─► probed(result, notice?)
//  probing(ready, no grant) ─► saved key? ─► reauthorizing ─► connected(restored)
//                                   │                 └──(verified ending)──► probed(ready_unlinked, notice)
//                                   └─ several keys, no hint ─► choose_credential
//  probed(ready_unlinked) ──connectBrowser() (user gesture)──► linking ─► (grant) ─► probing ─► connected
//  probed(*) re-probes every 3 s while on the connect screen; recheck() probes now.
//
// Recovery never starts a link, creates an instance or selects a workspace,
// and never discards a key on a network failure, timeout or 429. Timer
// re-probes do not repeat a recovery that already failed in this document;
// an explicit Recheck does.
import { createLink, EnrollmentMismatch, pollLink, type LinkEnrollment, type LinkRequest } from '../client/links';
import { loopbackPermission, probe as defaultProbe, reauthBinding, type LoopbackPermission, type ProbeResult } from '../client/health';
import { DaemonUnreachable, LinkDenied, LinkExpired, LinkGone, isAbortError } from '../client/errors';
import { currentGrant, setGrant, takeAuthorizationEnd, type Grant } from '../client/grant';
import { DAEMON_HTTP } from '../client/http';
import { Subject, type Observable } from '../client/observable';
import { CredentialStoreFull, credentialStore, type CredentialRecord, type CredentialStore, type NewCandidate } from '../client/credentialStore';
import { isRecoverySuppressed, recoverCredential, type RecoveryOutcome } from '../client/reauth';
import type { AuthEndState, RecoveryState } from '../client/recovery';
import { clearResume, readResumeHint, setResumeBinding, type ResumeBinding, type ResumeHint } from '../state/session';
import type { BrowserHealth } from '../client/types';

export const REPROBE_INTERVAL_MS = 3_000;

export type LinkStatus = 'creating' | 'waiting' | 'denied' | 'expired' | 'error';

/** A C7 explanation attached to the connect screen. */
export interface ConnectNotice {
  state: RecoveryState;
  /** `rate_limited` only: seconds before the automatic retry. */
  retryAfterSec?: number;
}

/** What a saved-authorization choice may show: listener and expiry, never paths or session content. */
export interface SavedAuthorization {
  credentialId: string;
  listener: string;
  expiresAt: string | null;
}

export type ConnectState =
  | { phase: 'idle' }
  | { phase: 'needs_permission' }
  | { phase: 'probing'; previous: ProbeResult | null; notice?: ConnectNotice | null }
  | { phase: 'probed'; result: ProbeResult; notice?: ConnectNotice | null }
  /** A saved key is being proven to the reachable daemon (C7 `reauthorizing`). */
  | { phase: 'reauthorizing'; health: BrowserHealth }
  /** Several saved authorizations match and no tab hint selects one (`recover`), or eight are saved (`forget`). */
  | { phase: 'choose_credential'; health: BrowserHealth; purpose: 'recover' | 'forget'; choices: SavedAuthorization[] }
  | { phase: 'linking'; health: BrowserHealth; status: LinkStatus; link: LinkRequest | null; error?: string }
  /** `restored`: recovered by proof after a reload (C7 `restored`), not a fresh link. */
  | { phase: 'connected'; grant: Grant; health: BrowserHealth; restored?: boolean };

export interface ConnectDeps {
  probe?: (signal?: AbortSignal) => Promise<ProbeResult>;
  permission?: () => Promise<LoopbackPermission>;
  createLink?: (signal?: AbortSignal, enrollment?: LinkEnrollment) => Promise<LinkRequest>;
  pollLink?: (request: LinkRequest, signal: AbortSignal, onPending: () => void) => Promise<Grant>;
  /** A page served by the daemon's hostname needs no cross-network permission. */
  pageSharesDaemonHost?: () => boolean;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
  /** Saved-key store (default: the document's IndexedDB store). */
  store?: CredentialStore;
  /** Prove one saved record (default: reauth.recoverCredential). */
  recover?: (record: CredentialRecord, signal: AbortSignal) => Promise<RecoveryOutcome>;
  /** This tab's resume hint seam (default: sessionStorage via state/session). */
  readHint?: () => ResumeHint | null;
  clearHint?: () => void;
  setBinding?: (binding: ResumeBinding | null) => void;
  /** Trusted page origin (default: location.origin). */
  pageOrigin?: () => string;
  now?: () => number;
}

function defaultPageSharesDaemonHost(): boolean {
  return typeof location !== 'undefined' && !!DAEMON_HTTP && new URL(DAEMON_HTTP).hostname === location.hostname;
}

function defaultPageOrigin(): string {
  return typeof location !== 'undefined' && typeof location.origin === 'string' ? location.origin : '';
}

type RecoveryStep = { done: true } | { done: false; notice: ConnectNotice | null };

const DONE: RecoveryStep = { done: true };
const notice = (state: RecoveryState | null, retryAfterSec?: number): RecoveryStep => ({ done: false, notice: state ? { state, ...(retryAfterSec ? { retryAfterSec } : {}) } : null });

export class ConnectController {
  private readonly subject = new Subject<ConnectState>({ phase: 'idle' });
  readonly state: Observable<ConnectState> = this.subject;
  private timer: unknown = null;
  private probeAbort: AbortController | null = null;
  private linkAbort: AbortController | null = null;
  private disposed = false;
  private readonly deps: Required<Omit<ConnectDeps, 'store'>>;
  private storeOverride: CredentialStore | undefined;
  /** Credentials whose recovery already failed in this document (no automatic repeat). */
  private readonly failed = new Set<string>();
  /**
   * A `daemon stopping` close was observed: the daemon epoch it ended under.
   * Kept while that daemon is unreachable or still answering (draining);
   * cleared once another daemon generation answers.
   */
  private stoppedEpoch: string | null = null;
  private stopped = false;
  /** Daemon epoch of the last connected health (to recognise a still-draining daemon). */
  private connectedEpoch: string | null = null;
  /** The user has seen `memory_only` and chose to connect for this page. */
  private memoryOnly = false;

  constructor(deps: ConnectDeps = {}) {
    this.storeOverride = deps.store;
    this.deps = {
      probe: deps.probe ?? (signal => defaultProbe(signal)),
      permission: deps.permission ?? (() => loopbackPermission()),
      createLink: deps.createLink ?? ((signal, enrollment) => createLink(undefined, signal, enrollment)),
      pollLink: deps.pollLink ?? ((request, signal, onPending) => pollLink(request, signal, { onPending })),
      pageSharesDaemonHost: deps.pageSharesDaemonHost ?? defaultPageSharesDaemonHost,
      setTimer: deps.setTimer ?? ((fn, ms) => setTimeout(fn, ms)),
      clearTimer: deps.clearTimer ?? (handle => clearTimeout(handle as ReturnType<typeof setTimeout>)),
      recover: deps.recover ?? ((record, signal) => recoverCredential(record, { store: this.store, signal })),
      readHint: deps.readHint ?? (() => readResumeHint()),
      clearHint: deps.clearHint ?? (() => clearResume()),
      setBinding: deps.setBinding ?? (binding => setResumeBinding(binding)),
      pageOrigin: deps.pageOrigin ?? defaultPageOrigin,
      now: deps.now ?? Date.now,
    };
  }

  private get store(): CredentialStore {
    return this.storeOverride ?? credentialStore();
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

  /** 'Recheck' button and post-link verification: also retries a failed recovery. */
  recheck(): Promise<void> {
    this.failed.clear();
    return this.probeAndRecover(false);
  }

  private async probeAndRecover(automatic: boolean): Promise<void> {
    if (this.disposed) return;
    const current = this.subject.get();
    if (current.phase === 'linking' && (current.status === 'creating' || current.status === 'waiting')) return;
    this.clearTimer();
    this.probeAbort?.abort();
    const abort = new AbortController();
    this.probeAbort = abort;
    const previous = current.phase === 'probed' ? current.result : current.phase === 'probing' ? current.previous : null;
    const previousNotice = current.phase === 'probed' || current.phase === 'probing' ? (current.notice ?? null) : null;
    this.subject.set({ phase: 'probing', previous, notice: previousNotice });
    let result: ProbeResult;
    try {
      result = await this.deps.probe(abort.signal);
    } catch (error) {
      if (abort.signal.aborted || this.disposed) return;
      result = { kind: 'unreachable', detail: isAbortError(error) ? 'timeout' : 'network' };
    }
    if (abort.signal.aborted || this.disposed) return;
    const ended = takeAuthorizationEnd();
    if (ended === 'daemon_stopped') {
      this.stopped = true;
      this.stoppedEpoch = this.connectedEpoch;
    }
    const grant = currentGrant();
    if (result.kind === 'ready' && grant) {
      this.probeAbort = null;
      this.connect(grant, result.health, false);
      return;
    }
    if (result.kind === 'ready' && !grant) result = { kind: 'ready_unlinked', health: result.health };
    let step: RecoveryStep;
    const answeringEpoch = result.kind === 'ready_unlinked' ? (result.health.browser_reauth?.daemon_epoch ?? null) : null;
    if (result.kind === 'ready_unlinked' && this.stopped && (this.stoppedEpoch === null || answeringEpoch === this.stoppedEpoch)) {
      // The stopping daemon still answers while it drains: never prove a
      // saved key against it (its generation is going away).
      step = notice('daemon_stopped');
    } else if (result.kind === 'ready_unlinked') {
      this.stopped = false;
      this.stoppedEpoch = null;
      step = await this.recover(result.health, ended, automatic, abort.signal);
    } else if (result.kind === 'unreachable') {
      step = notice(this.stopped ? 'daemon_stopped' : (await this.hasSavedCredential()) ? 'daemon_unreachable' : null);
    } else {
      step = notice(ended && ended !== 'daemon_unreachable' ? ended : null);
    }
    if (step.done || abort.signal.aborted || this.disposed) return;
    this.probeAbort = null;
    this.subject.set({ phase: 'probed', result, notice: step.notice });
    this.scheduleReprobe(step.notice?.retryAfterSec ? step.notice.retryAfterSec * 1000 : REPROBE_INTERVAL_MS);
  }

  private async hasSavedCredential(): Promise<boolean> {
    try {
      return (await this.store.load()).usable.length > 0;
    } catch {
      return false;
    }
  }

  /** Select and prove a saved key for this reachable daemon (C4/C5 order). */
  private async recover(health: BrowserHealth, ended: AuthEndState | null, automatic: boolean, signal: AbortSignal): Promise<RecoveryStep> {
    // A classified ending of this document's grant already removed (or
    // suppressed) its key: show that reason rather than another attempt.
    if (ended && ended !== 'daemon_unreachable' && ended !== 'daemon_stopped') return notice(ended);
    const loaded = await this.store.load();
    if (signal.aborted) return DONE;
    if (!loaded.available || loaded.usable.length === 0) return notice(loaded.foreignOrigin ? 'origin_mismatch' : null);
    const binding = reauthBinding(health);
    if (!binding) return notice('reauth_unsupported');
    const hint = this.deps.readHint();
    let restarted = false;
    let changed = false;
    const current: CredentialRecord[] = [];
    for (const record of loaded.usable) {
      if (record.daemonEpoch !== binding.daemonEpoch) restarted = true;
      else if (record.listener !== binding.listener || record.listenerEpoch !== binding.listenerEpoch) changed = true;
      else {
        current.push(record);
        continue;
      }
      await this.store.remove(record.credentialId);
    }
    if (hint && !current.some(record => record.credentialId === hint.binding.credentialId)) {
      // Never restore a coincidentally reused ID after a restart or rebind.
      this.deps.clearHint();
    }
    const candidates = current.filter(record => !isRecoverySuppressed(record.credentialId) && !(automatic && this.failed.has(record.credentialId)));
    if (!candidates.length) {
      if (restarted) return notice('daemon_restarted');
      if (changed) return notice('listener_changed');
      return notice(current.length ? 'authorization_unavailable' : null);
    }
    const hinted = hint
      ? candidates.find(record => record.credentialId === hint.binding.credentialId && record.daemonEpoch === hint.binding.daemonEpoch && record.listenerEpoch === hint.binding.listenerEpoch)
      : undefined;
    const chosen = hinted ?? (candidates.length === 1 ? candidates[0] : undefined);
    if (!chosen) {
      this.subject.set({ phase: 'choose_credential', health, purpose: 'recover', choices: candidates.map(summary) });
      return DONE;
    }
    return this.prove(chosen, health, signal);
  }

  private async prove(record: CredentialRecord, health: BrowserHealth, signal: AbortSignal): Promise<RecoveryStep> {
    this.subject.set({ phase: 'reauthorizing', health });
    let outcome: RecoveryOutcome;
    try {
      outcome = await this.deps.recover(record, signal);
    } catch (error) {
      if (signal.aborted) return DONE;
      outcome = error instanceof DaemonUnreachable ? { kind: 'unreachable', detail: error.detail } : { kind: 'invalid' };
    }
    if (signal.aborted || this.disposed) return DONE;
    const dropHint = () => {
      if (this.deps.readHint()?.binding.credentialId === record.credentialId) this.deps.clearHint();
    };
    switch (outcome.kind) {
      case 'granted':
        setGrant(outcome.grant);
        this.deps.setBinding(outcome.grant.reauth ?? null);
        this.probeAbort = null;
        this.connect(outcome.grant, health, true);
        return DONE;
      case 'ended':
        this.failed.add(record.credentialId);
        if (outcome.state !== 'origin_mismatch') dropHint();
        return notice(outcome.state);
      case 'rate_limited':
        return notice('rate_limited', outcome.retryAfterSec);
      case 'unreachable':
        return notice('daemon_unreachable');
      case 'failed':
      case 'invalid':
      case 'no_credential':
        this.failed.add(record.credentialId);
        dropHint();
        return notice('authorization_unavailable');
    }
  }

  private connect(grant: Grant, health: BrowserHealth, restored: boolean): void {
    this.connectedEpoch = health.browser_reauth?.daemon_epoch ?? null;
    this.subject.set({ phase: 'connected', grant, health, restored });
  }

  /** Choose one saved authorization (several matched and no tab hint selected one). */
  async chooseCredential(credentialId: string): Promise<void> {
    const current = this.subject.get();
    if (current.phase !== 'choose_credential' || current.purpose !== 'recover' || !current.choices.some(choice => choice.credentialId === credentialId)) return;
    const record = (await this.store.load()).usable.find(candidate => candidate.credentialId === credentialId);
    if (!record) return void this.recheck();
    this.clearTimer();
    this.probeAbort?.abort();
    const abort = new AbortController();
    this.probeAbort = abort;
    const step = await this.prove(record, current.health, abort.signal);
    if (step.done || abort.signal.aborted || this.disposed) return;
    this.probeAbort = null;
    this.subject.set({ phase: 'probed', result: { kind: 'ready_unlinked', health: current.health }, notice: step.notice });
    this.scheduleReprobe(step.notice?.retryAfterSec ? step.notice.retryAfterSec * 1000 : REPROBE_INTERVAL_MS);
  }

  /**
   * “Forget this browser” for one saved authorization (local only: it cannot
   * revoke another tab's live bearer; owner revocation is `browsers.delete`).
   */
  async forgetCredential(credentialId: string): Promise<void> {
    await this.store.remove(credentialId);
    if (this.deps.readHint()?.binding.credentialId === credentialId) this.deps.clearHint();
    const current = this.subject.get();
    if (current.phase === 'choose_credential' && current.purpose === 'forget') {
      this.subject.set({ phase: 'probed', result: { kind: 'ready_unlinked', health: current.health }, notice: null });
      return this.connectBrowser();
    }
    await this.recheck();
  }

  /** 'Cancel' while reauthorizing: stop this attempt and offer the normal choices. */
  cancelRecovery(): void {
    const current = this.subject.get();
    if (current.phase !== 'reauthorizing' && current.phase !== 'choose_credential') return;
    this.probeAbort?.abort();
    this.probeAbort = null;
    this.subject.set({ phase: 'probed', result: { kind: 'ready_unlinked', health: current.health }, notice: null });
  }

  /**
   * 'Connect browser' (user gesture): enroll a fresh key (never a stored one)
   * and wait for owner approval. Without key persistence the first press
   * shows `memory_only`; pressing again connects for this page only.
   */
  async connectBrowser(): Promise<void> {
    const current = this.subject.get();
    const health =
      current.phase === 'probed' && (current.result.kind === 'ready_unlinked' || current.result.kind === 'ready')
        ? current.result.health
        : current.phase === 'linking' || current.phase === 'choose_credential' || current.phase === 'reauthorizing'
          ? current.health
          : null;
    if (!health || this.disposed) return;
    if (current.phase === 'probed' && current.notice?.state === 'memory_only') this.memoryOnly = true;
    this.clearTimer();
    this.probeAbort?.abort();
    this.linkAbort?.abort();
    const abort = new AbortController();
    this.linkAbort = abort;
    const binding = reauthBinding(health);
    let candidate: NewCandidate | null = null;
    if (binding && !this.memoryOnly) {
      try {
        await this.store.purgeOtherGenerations(binding);
        candidate = await this.store.createCandidate(binding);
      } catch (error) {
        if (error instanceof CredentialStoreFull) {
          const { usable } = await this.store.load();
          this.subject.set({ phase: 'choose_credential', health, purpose: 'forget', choices: usable.map(summary) });
          return;
        }
        candidate = null;
      }
      if (abort.signal.aborted) return;
      if (!candidate) {
        this.subject.set({ phase: 'probed', result: { kind: 'ready_unlinked', health }, notice: { state: 'memory_only' } });
        return;
      }
    }
    this.subject.set({ phase: 'linking', health, status: 'creating', link: null });
    const enrollment: LinkEnrollment | undefined =
      candidate && binding
        ? { credentialId: candidate.record.credentialId, publicKey: candidate.publicKey, ...binding, origin: this.deps.pageOrigin() }
        : undefined;
    let link: LinkRequest;
    try {
      link = await this.deps.createLink(abort.signal, enrollment);
    } catch (error) {
      if (abort.signal.aborted) return;
      // A lost create response may still have registered the key: keep it for
      // one proof on the next load. A refusal or mismatch discards it.
      if (candidate && !(error instanceof DaemonUnreachable)) await this.store.remove(candidate.record.credentialId);
      if (error instanceof EnrollmentMismatch) {
        this.linkAbort = null;
        this.subject.set({ phase: 'probing', previous: null });
        return this.recheck();
      }
      this.subject.set({ phase: 'linking', health, status: 'error', link: null, error: error instanceof Error ? error.message : 'Could not start linking.' });
      return;
    }
    if (abort.signal.aborted) return;
    if (candidate) await this.store.update(candidate.record.credentialId, { linkExpiresAt: link.expiresAt });
    this.subject.set({ phase: 'linking', health, status: 'waiting', link });
    let grant: Grant;
    try {
      grant = await this.deps.pollLink(link, abort.signal, () => undefined);
    } catch (error) {
      if (abort.signal.aborted || isAbortError(error)) return;
      if (error instanceof LinkGone && candidate) {
        // Another same-origin tab may have collected this approval by proof (C2/F6).
        const candidateId = candidate.record.credentialId;
        const record = (await this.store.load()).usable.find(r => r.credentialId === candidateId);
        const outcome = record ? await this.deps.recover(record, abort.signal).catch(() => ({ kind: 'invalid' }) as RecoveryOutcome) : null;
        if (abort.signal.aborted) return;
        if (outcome?.kind === 'granted') {
          setGrant(outcome.grant);
          this.deps.setBinding(outcome.grant.reauth ?? null);
          this.linkAbort = null;
          this.subject.set({ phase: 'probing', previous: null });
          return this.recheck();
        }
      }
      if (candidate && (error instanceof LinkDenied || error instanceof LinkExpired || error instanceof LinkGone)) await this.store.remove(candidate.record.credentialId);
      const status: LinkStatus = error instanceof LinkDenied ? 'denied' : error instanceof LinkExpired || error instanceof LinkGone ? 'expired' : 'error';
      this.subject.set({ phase: 'linking', health, status, link, error: error instanceof Error ? error.message : undefined });
      return;
    }
    if (candidate) await this.store.update(candidate.record.credentialId, { grantId: grant.id, expiresAt: grant.expiresAt });
    this.deps.setBinding(grant.reauth ?? null);
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
    // strand the screen with no poll and no re-probe. A cancelled candidate
    // stays saved: the owner may still approve it before its deadline.
    const current = this.subject.get();
    if (current.phase === 'linking') this.subject.set({ phase: 'probing', previous: { kind: 'ready_unlinked', health: current.health } });
    void this.recheck();
  }

  /** After an authorization ending or a fatal engine loss: back to the connect screen. */
  reset(): void {
    this.linkAbort?.abort();
    this.probeAbort?.abort();
    this.clearTimer();
    this.subject.set({ phase: 'probing', previous: null });
    void this.probeAndRecover(false);
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimer();
    this.probeAbort?.abort();
    this.linkAbort?.abort();
  }

  private scheduleReprobe(ms: number = REPROBE_INTERVAL_MS): void {
    this.clearTimer();
    this.timer = this.deps.setTimer(() => {
      this.timer = null;
      const state = this.subject.get();
      if (state.phase === 'probed') void this.probeAndRecover(true);
    }, ms);
  }

  private clearTimer(): void {
    if (this.timer !== null) this.deps.clearTimer(this.timer);
    this.timer = null;
  }
}

function summary(record: CredentialRecord): SavedAuthorization {
  return { credentialId: record.credentialId, listener: record.listener, expiresAt: record.expiresAt };
}
