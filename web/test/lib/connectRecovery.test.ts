import { afterEach, describe, expect, it, vi } from 'vitest';
import { CredentialStore, memoryBackend, type CredentialRecord } from '@/lib/client/credentialStore';
import { LinkGone } from '@/lib/client/errors';
import { REAUTH_CAPABILITY, REQUIRED_CAPABILITIES, reauthBinding, type ProbeResult } from '@/lib/client/health';
import { clearGrant, currentGrant, endAuthorization, setGrant, takeAuthorizationEnd, type Grant } from '@/lib/client/grant';
import type { LinkEnrollment, LinkRequest } from '@/lib/client/links';
import { acquirePrimaryLock, primaryLockName, webLocker } from '@/lib/client/primaryLock';
import { resetRecoveryStateForTests, suppressRecovery, type RecoveryOutcome } from '@/lib/client/reauth';
import { RECOVERY_COPY, type RecoveryState } from '@/lib/client/recovery';
import type { BrowserHealth } from '@/lib/client/types';
import { ConnectController, REPROBE_INTERVAL_MS, type ConnectDeps, type ConnectState } from '@/lib/connect/controller';
import { RECOVERY_TITLES, connectView } from '@/lib/connect/model';
import type { ResumeBinding, ResumeHint } from '@/lib/state/session';
import { flush } from './helpers';

const ORIGIN = 'http://127.0.0.1:3000';
const BASE = 'http://127.0.0.1:9877';
const D = 'Gs9c1yhW-SA6P6HLQUsNew';
const L = 'C0tOZXEBuALBXtZAsgmDCg';
const HEALTH: BrowserHealth = {
  status: 'ok',
  build: '0.160.0',
  protocols: { api: 1, frontend: 1, terminal: 1, browser: 1, browser_reauth: 1 },
  capabilities: [...REQUIRED_CAPABILITIES, REAUTH_CAPABILITY],
  listener: { name: 'loopback', transport: 'loopback_http', features: ['engine'] },
  engine: { state: 'on' },
  browser_links: true,
  browser_reauth: { version: 1, daemon_epoch: D, listener_epoch: L },
};
const READY: ProbeResult = { kind: 'ready', health: HEALTH };

function grantFor(record: CredentialRecord): Grant {
  return { id: 'brw_0123', token: `awb_${'G'.repeat(43)}`, scope: 'engine', origin: ORIGIN, listener: 'loopback', expiresAt: '2999-01-01T00:00:00.000Z', reauth: { credentialId: record.credentialId, daemonEpoch: record.daemonEpoch, listenerEpoch: record.listenerEpoch } };
}

async function harness(options: { results?: ProbeResult[]; records?: Partial<{ daemonEpoch: string; listenerEpoch: string; listener: string }>[]; outcome?: (record: CredentialRecord) => RecoveryOutcome; hint?: (records: CredentialRecord[]) => ResumeHint | null; backend?: ReturnType<typeof memoryBackend> } = {}) {
  const backend = options.backend ?? memoryBackend();
  const store = new CredentialStore(backend, { origin: ORIGIN, daemonBase: BASE });
  const records: CredentialRecord[] = [];
  for (const binding of options.records ?? []) records.push((await store.createCandidate({ listener: 'loopback', daemonEpoch: D, listenerEpoch: L, ...binding }))!.record);
  let hint = options.hint?.(records) ?? null;
  const timers: { fn: () => void; ms: number }[] = [];
  const recover = vi.fn(async (record: CredentialRecord) => (options.outcome ?? (r => ({ kind: 'granted', grant: grantFor(r) }) as RecoveryOutcome))(record));
  const setBinding = vi.fn((_binding: ResumeBinding | null) => undefined);
  const createLink = vi.fn(async (_signal?: AbortSignal, enrollment?: LinkEnrollment): Promise<LinkRequest> => ({
    id: 'blk_1',
    code: 'web-K7PM-3Q9X',
    pollingSecret: 'S'.repeat(43),
    intervalMs: 2000,
    expiresAt: '2999-01-01T00:05:00.000Z',
    listener: 'loopback',
    origin: ORIGIN,
    ...(enrollment ? { reauth: { credentialId: enrollment.credentialId, daemonEpoch: enrollment.daemonEpoch, listenerEpoch: enrollment.listenerEpoch } } : {}),
  }));
  let probes = 0;
  const results = options.results ?? [READY];
  const deps: ConnectDeps = {
    probe: async () => results[Math.min(probes++, results.length - 1)]!,
    permission: async () => 'unknown',
    pageSharesDaemonHost: () => false,
    setTimer: (fn, ms) => timers.push({ fn, ms }),
    clearTimer: () => timers.splice(0),
    store,
    recover: async record => recover(record),
    readHint: () => hint,
    clearHint: () => (hint = null),
    setBinding,
    pageOrigin: () => ORIGIN,
    createLink,
  };
  const controller = new ConnectController(deps);
  return { controller, backend, store, records, timers, recover, setBinding, createLink, hint: () => hint, deps };
}

const state = (c: ConnectController) => c.state.get();
const noticeOf = (s: ConnectState) => (s.phase === 'probed' ? (s.notice?.state ?? null) : null);

afterEach(() => {
  clearGrant();
  takeAuthorizationEnd();
  resetRecoveryStateForTests();
});

describe('reload recovery (C3–C5)', () => {
  it('a reachable daemon and a matching key reconnect with no new link and bind the resume hint', async () => {
    const h = await harness({ records: [{}] });
    const seen: string[] = [];
    h.controller.state.subscribe(s => seen.push(s.phase));
    await h.controller.start();
    expect(seen).toContain('reauthorizing');
    const s = state(h.controller);
    expect(s).toMatchObject({ phase: 'connected', restored: true });
    expect(currentGrant()?.id).toBe('brw_0123');
    expect(h.createLink).not.toHaveBeenCalled();
    expect(h.setBinding).toHaveBeenCalledWith({ credentialId: h.records[0]!.credentialId, daemonEpoch: D, listenerEpoch: L });
    expect(connectView(s)).toMatchObject({ kind: 'connected', notice: 'restored', body: [RECOVERY_COPY.restored] });
  });

  it('several saved authorizations: the tab hint selects one; without a hint the user chooses', async () => {
    const hinted = await harness({ records: [{}, {}], hint: records => ({ record: { instanceId: 'ins_1', frontendId: 'frn_1', tabId: null, launchContext: null }, binding: { credentialId: records[1]!.credentialId, daemonEpoch: D, listenerEpoch: L } }) });
    await hinted.controller.start();
    expect(hinted.recover).toHaveBeenCalledWith(expect.objectContaining({ credentialId: hinted.records[1]!.credentialId }));
    clearGrant();
    const choose = await harness({ records: [{}, {}] });
    await choose.controller.start();
    const s = state(choose.controller);
    expect(s).toMatchObject({ phase: 'choose_credential', purpose: 'recover' });
    expect(choose.recover).not.toHaveBeenCalled();
    const view = connectView(s);
    expect(view.choices.map(c => Object.keys(c).sort())).toEqual([['credentialId', 'expiresAt', 'listener'], ['credentialId', 'expiresAt', 'listener']]);
    await choose.controller.chooseCredential(choose.records[0]!.credentialId);
    expect(state(choose.controller)).toMatchObject({ phase: 'connected', restored: true });
  });

  it('daemon restart: removes the key and the hint, then shows daemon_restarted', async () => {
    const h = await harness({ records: [{ daemonEpoch: 'AAAAAAAAAAAAAAAAAAAAAA' }], hint: records => ({ record: { instanceId: 'ins_1', frontendId: 'frn_1', tabId: null, launchContext: null }, binding: { credentialId: records[0]!.credentialId, daemonEpoch: 'AAAAAAAAAAAAAAAAAAAAAA', listenerEpoch: L } }) });
    await h.controller.start();
    expect(noticeOf(state(h.controller))).toBe('daemon_restarted');
    expect(h.backend.data.size).toBe(0);
    expect(h.hint()).toBeNull();
    expect(h.recover).not.toHaveBeenCalled();
    expect(connectView(state(h.controller))).toMatchObject({ kind: 'daemon_restarted', actions: ['connect', 'recheck'] });
  });

  it('listener rebind: listener_changed', async () => {
    const h = await harness({ records: [{ listenerEpoch: 'AAAAAAAAAAAAAAAAAAAAAA' }] });
    await h.controller.start();
    expect(noticeOf(state(h.controller))).toBe('listener_changed');
  });

  it('unreachable with a saved key: daemon_unreachable keeps the key (never "restarted" from a timeout)', async () => {
    const h = await harness({ records: [{}], results: [{ kind: 'unreachable', detail: 'timeout' }] });
    await h.controller.start();
    expect(noticeOf(state(h.controller))).toBe('daemon_unreachable');
    expect(h.backend.data.size).toBe(1);
    const view = connectView(state(h.controller));
    expect(view.body[0]).toBe(RECOVERY_COPY.daemon_unreachable);
    expect(view.recipe).not.toBeNull();
  });

  it.each([
    ['authorization_expired', { kind: 'ended', state: 'authorization_expired' }],
    ['authorization_revoked', { kind: 'ended', state: 'authorization_revoked' }],
    ['authorization_unavailable', { kind: 'failed' }],
    ['rate_limited', { kind: 'rate_limited', retryAfterSec: 9 }],
    ['daemon_unreachable', { kind: 'unreachable', detail: 'network' }],
    ['origin_mismatch', { kind: 'ended', state: 'origin_mismatch' }],
  ] as const)('proof outcome → %s, never an automatic link', async (expected, outcome) => {
    const h = await harness({ records: [{}], outcome: () => outcome as RecoveryOutcome });
    await h.controller.start();
    expect(noticeOf(state(h.controller))).toBe(expected);
    expect(h.createLink).not.toHaveBeenCalled();
    expect(currentGrant()).toBeNull();
    if (expected === 'rate_limited') expect(h.timers.at(-1)?.ms).toBe(9000);
  });

  it('a failed recovery is not repeated by the 3 s re-probe; Recheck retries it', async () => {
    const h = await harness({ records: [{}], outcome: () => ({ kind: 'failed' }) });
    await h.controller.start();
    expect(h.timers.at(-1)?.ms).toBe(REPROBE_INTERVAL_MS);
    h.timers.at(-1)!.fn();
    await flush();
    await flush();
    expect(h.recover).toHaveBeenCalledTimes(1);
    await h.controller.recheck();
    expect(h.recover).toHaveBeenCalledTimes(2);
  });

  it('a suppressed credential (same bearer refused after proof) is not recovered again', async () => {
    const h = await harness({ records: [{}] });
    suppressRecovery(h.records[0]!.credentialId);
    await h.controller.start();
    expect(h.recover).not.toHaveBeenCalled();
    expect(noticeOf(state(h.controller))).toBe('authorization_unavailable');
  });

  it('an ended authorization shows its exact state after reset, without another proof', async () => {
    const h = await harness({ records: [{}] });
    await h.controller.start();
    endAuthorization('authorization_revoked');
    h.controller.reset();
    await flush();
    await flush();
    expect(noticeOf(state(h.controller))).toBe('authorization_revoked');
    expect(h.recover).toHaveBeenCalledTimes(1);
  });

  it('daemon stopping is remembered while the daemon is unreachable', async () => {
    const h = await harness({ records: [{}], results: [READY, { kind: 'unreachable', detail: 'network' }] });
    await h.controller.start();
    endAuthorization('daemon_stopped');
    h.controller.reset();
    for (let i = 0; i < 4; i++) await flush();
    expect(noticeOf(state(h.controller))).toBe('daemon_stopped');
    expect(connectView(state(h.controller))).toMatchObject({ kind: 'daemon_stopped', startCommand: 'awman daemon start --background', actions: ['recheck'] });
  });

  it('a daemon without browser_reauth cannot silently downgrade a saved credential', async () => {
    const legacy = { ...HEALTH, protocols: { api: 1, frontend: 1, terminal: 1, browser: 1 }, capabilities: [...REQUIRED_CAPABILITIES] } as BrowserHealth;
    delete legacy.browser_reauth;
    expect(reauthBinding(legacy)).toBeNull();
    const h = await harness({ records: [{}], results: [{ kind: 'ready', health: legacy }] });
    await h.controller.start();
    expect(noticeOf(state(h.controller))).toBe('reauth_unsupported');
    expect(h.backend.data.size).toBe(1);
  });

  it('no saved key: the ordinary unlinked state (first-time linking)', async () => {
    const h = await harness();
    await h.controller.start();
    expect(state(h.controller)).toMatchObject({ phase: 'probed', result: { kind: 'ready_unlinked' }, notice: null });
  });
});

describe('linking with a saved key (C1, C2)', () => {
  it('enrolls a fresh persisted key, records the link deadline and the grant, then binds the hint', async () => {
    const h = await harness();
    let poll!: (grant: Grant) => void;
    h.deps.pollLink = undefined;
    const controller = new ConnectController({ ...h.deps, pollLink: () => new Promise(resolve => (poll = resolve)) });
    await controller.start();
    const linking = controller.connectBrowser();
    await vi.waitFor(() => expect(state(controller)).toMatchObject({ phase: 'linking', status: 'waiting' }));
    const enrollment = h.createLink.mock.calls[0]![1]!;
    expect(enrollment).toMatchObject({ listener: 'loopback', daemonEpoch: D, listenerEpoch: L, origin: ORIGIN });
    const stored = h.backend.data.get(enrollment.credentialId) as CredentialRecord;
    expect(stored.publicKey).toBe(enrollment.publicKey);
    expect(stored.linkExpiresAt).toBe('2999-01-01T00:05:00.000Z');
    const grant = grantFor(stored);
    setGrant(grant);
    poll(grant);
    await linking;
    expect((h.backend.data.get(enrollment.credentialId) as CredentialRecord).grantId).toBe('brw_0123');
    expect(h.setBinding).toHaveBeenLastCalledWith(grant.reauth);
    expect(state(controller)).toMatchObject({ phase: 'connected', restored: false });
  });

  it('memory-only fallback: first press explains, second press links keyless', async () => {
    const backend = memoryBackend();
    backend.put = async () => {
      throw new DOMException('quota', 'QuotaExceededError');
    };
    const h = await harness({ backend });
    await h.controller.start();
    await h.controller.connectBrowser();
    expect(noticeOf(state(h.controller))).toBe('memory_only');
    expect(connectView(state(h.controller))).toMatchObject({ kind: 'memory_only', body: [RECOVERY_COPY.memory_only, expect.any(String)], actions: ['connect', 'recheck'] });
    expect(h.createLink).not.toHaveBeenCalled();
    void h.controller.connectBrowser();
    await vi.waitFor(() => expect(h.createLink).toHaveBeenCalledTimes(1));
    expect(h.createLink.mock.calls[0]![1]).toBeUndefined();
  });

  it('link_gone for a key-bound candidate tries proof once before reporting failure', async () => {
    const h = await harness();
    const controller = new ConnectController({ ...h.deps, pollLink: async () => Promise.reject(new LinkGone()) });
    await controller.start();
    await controller.connectBrowser();
    expect(h.recover).toHaveBeenCalledTimes(1);
    expect(state(controller)).toMatchObject({ phase: 'connected' });
  });

  it('eight saved authorizations: asks the user to forget one instead of evicting', async () => {
    const h = await harness({ records: Array.from({ length: 8 }, () => ({})) });
    const controller = new ConnectController({ ...h.deps, recover: async () => ({ kind: 'failed' }) });
    await controller.start();
    await controller.connectBrowser();
    expect(state(controller)).toMatchObject({ phase: 'choose_credential', purpose: 'forget' });
    expect(h.backend.data.size).toBe(8);
    expect(h.createLink).not.toHaveBeenCalled();
  });
});

describe('C7 copy', () => {
  const TABLE: Record<RecoveryState, string> = {
    reauthorizing: 'Reconnecting this browser using its saved authorization. No new link approval is needed.',
    restored: 'Reconnected to your workspace. Commands and answers were not resent. Check the current status before repeating an action.',
    authorization_expired: "This browser's 12-hour authorization has expired. Connect this browser again and approve the new link command.",
    authorization_revoked: "This browser's access was revoked. Connect this browser again only if the owner wants to restore access.",
    daemon_restarted: 'The daemon has restarted since this browser was linked. Its previous authorization is no longer valid. Connect this browser again.',
    daemon_stopped: 'The daemon is stopping or has stopped. Start it on this machine, then choose Recheck. You will need to link this browser again after it restarts.',
    daemon_unreachable: 'The daemon did not answer. It may be stopped, its listener may be off, or the browser may be blocking access. Check awman daemon status, then choose Recheck. Your saved authorization has not been discarded.',
    other_primary: 'Another browser tab or frontend holds primary control of this workspace. You can view it here, return to the primary tab, or open a new workspace.',
    listener_changed: 'The daemon listener has changed since this browser was linked. Connect this browser again on the intended listener.',
    listener_closed: "The daemon listener was disabled or changed, so this browser's authorization ended. Check the listener settings, then choose Recheck.",
    origin_removed: "This site's origin is no longer allowed by the daemon listener. Ask the owner to review the allowed origins, then choose Recheck.",
    origin_mismatch: 'This authorization belongs to a different site origin. Return to the original site or connect this browser here with a new approval.',
    authorization_unavailable: 'This browser could not recover its saved authorization. Connect this browser again and approve the new link command.',
    reauth_unsupported: 'This daemon does not support saved browser authorization for this app version. Update awman, then choose Recheck.',
    memory_only: 'This browser cannot save its authorization key. You can connect for this page, but reloading will require another link approval.',
    resume_unavailable: 'Your browser is authorized, but the previous workspace connection can no longer be resumed. Choose a workspace to continue. No command has been restarted.',
    tab_closed: "The previously selected tab has closed. Showing the workspace's current tab.",
    terminal_view_only: 'This terminal is view only after reconnecting. Request control to type or resize it.',
    rate_limited: 'Reconnection is temporarily rate limited. Waiting before trying again.',
  };

  it('every state paragraph is the literal C7 text and has a title', () => {
    expect(RECOVERY_COPY).toEqual(TABLE);
    for (const key of Object.keys(TABLE) as RecoveryState[]) expect(RECOVERY_TITLES[key]).toMatch(/\S/);
  });

  it('connect-screen notices lead with the C7 paragraph and keep the existing setup guidance', () => {
    for (const notice of ['authorization_expired', 'authorization_revoked', 'daemon_restarted', 'listener_changed', 'origin_mismatch', 'authorization_unavailable', 'reauth_unsupported', 'memory_only', 'listener_closed', 'origin_removed'] as const) {
      const view = connectView({ phase: 'probed', result: { kind: 'ready_unlinked', health: HEALTH }, notice: { state: notice } });
      expect(view.kind).toBe(notice);
      expect(view.notice).toBe(notice);
      expect(view.body[0]).toBe(TABLE[notice]);
      expect(view.actions).toContain('recheck');
    }
    expect(connectView({ phase: 'reauthorizing', health: HEALTH })).toMatchObject({ kind: 'reauthorizing', body: [TABLE.reauthorizing], actions: ['cancel_recovery'], busy: true });
    expect(connectView({ phase: 'probed', result: { kind: 'unreachable', detail: 'network' }, notice: { state: 'rate_limited', retryAfterSec: 30 } })).toMatchObject({ body: [TABLE.rate_limited], retryAfterSec: 30 });
    // Existing states keep their own copy.
    expect(connectView({ phase: 'probed', result: { kind: 'permission_denied' } }).body[0]).toMatch(/denied this page access/);
  });
});

describe('primary lock (C5)', () => {
  it('names the lock by epochs and instance as a compact JSON array', () => {
    expect(primaryLockName(D, L, 'ins_1')).toBe(`awman-primary:["${D}","${L}","ins_1"]`);
  });

  it('uses ifAvailable exclusive requests and never steals', async () => {
    const requests: unknown[] = [];
    let held = false;
    const api = {
      request: (name: string, options: object, callback: (lock: unknown) => Promise<void> | void) => {
        requests.push({ name, options });
        if (held) return Promise.resolve(callback(null));
        held = true;
        return Promise.resolve(callback({ name })).then(() => (held = false));
      },
    };
    const locker = webLocker(api);
    const first = await locker.tryAcquire('awman-primary:x');
    expect(first).not.toBeNull();
    expect(await locker.tryAcquire('awman-primary:x')).toBeNull();
    expect(requests.every(r => JSON.stringify((r as { options: object }).options) === '{"mode":"exclusive","ifAvailable":true}')).toBe(true);
    first!.release();
    await flush();
    expect(await locker.tryAcquire('awman-primary:x')).not.toBeNull();
    expect(webLocker(null).supported).toBe(false);
  });

  it('bounded retry: at most five seconds at the reconnect backoff', async () => {
    let now = 0;
    const attempts: number[] = [];
    const handle = await acquirePrimaryLock({ supported: true, tryAcquire: async () => (attempts.push(now), null) }, 'n', { retryForMs: 5000, now: () => now, sleep: async ms => void (now += ms) });
    expect(handle).toBeNull();
    expect(now).toBeLessThanOrEqual(5000);
    expect(attempts.slice(0, 3)).toEqual([0, 50, 150]);
  });
});

describe('daemon stopping (C7 daemon_stopped)', () => {
  it('never proves a saved key against the draining daemon; a new generation reports daemon_restarted', async () => {
    const restartedHealth = { ...HEALTH, browser_reauth: { version: 1 as const, daemon_epoch: 'AAAAAAAAAAAAAAAAAAAAAA', listener_epoch: L } };
    const h = await harness({ records: [{}], results: [READY, READY, { kind: 'ready', health: restartedHealth }] });
    await h.controller.start();
    expect(state(h.controller).phase).toBe('connected');
    clearGrant();
    endAuthorization('daemon_stopped');
    h.controller.reset();
    for (let i = 0; i < 6; i++) await flush();
    expect(noticeOf(state(h.controller))).toBe('daemon_stopped');
    expect(h.recover).toHaveBeenCalledTimes(1);
    expect(h.backend.data.size).toBe(1);
    await h.controller.recheck();
    expect(noticeOf(state(h.controller))).toBe('daemon_restarted');
    expect(h.backend.data.size).toBe(0);
  });
});
