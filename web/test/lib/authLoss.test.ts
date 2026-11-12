import { afterEach, describe, expect, it, vi } from 'vitest';
import { DaemonClient } from '@/lib/client/daemonClient';
import { GrantRevoked } from '@/lib/client/errors';
import { clearGrant, currentGrant, setGrant, takeAuthorizationEnd, type Grant } from '@/lib/client/grant';
import type { RecoveryOutcome } from '@/lib/client/reauth';
import { endStateForCloseReason } from '@/lib/client/recovery';
import { GRANT, fakeFetch } from './helpers';

const BOUND: Grant = { ...GRANT, reauth: { credentialId: 'Z'.repeat(42) + 'E', daemonEpoch: 'Gs9c1yhW-SA6P6HLQUsNew', listenerEpoch: 'C0tOZXEBuALBXtZAsgmDCg' } };
const refused = { status: 401, body: { code: 'browser_grant_invalid', error: 'invalid' } };
const ok = { status: 200, body: { pid: 1 } };

afterEach(() => {
  clearGrant();
  takeAuthorizationEnd();
});

function client(responses: { status: number; body: unknown }[], outcome: RecoveryOutcome | (() => Promise<RecoveryOutcome>)) {
  let index = 0;
  const { calls, transport } = fakeFetch(() => responses[Math.min(index++, responses.length - 1)]!);
  const recover = vi.fn(typeof outcome === 'function' ? outcome : async () => outcome);
  const forgetCredential = vi.fn(async () => undefined);
  setGrant(BOUND);
  const c = new DaemonClient(BOUND, transport, { recover, forgetCredential, unexplainedCloseDelayMs: 0 });
  const ended: string[] = [];
  c.onAuthEnded(state => ended.push(state));
  return { c, calls, recover, forgetCredential, ended };
}

describe('operational 401 classification (C3)', () => {
  it('proves once; the same bearer is accepted → the request is retried once', async () => {
    const h = client([refused, ok], { kind: 'granted', grant: BOUND });
    await expect(h.c.status()).resolves.toEqual({ pid: 1 });
    expect(h.recover).toHaveBeenCalledTimes(1);
    expect(h.calls).toHaveLength(2);
    expect(h.ended).toEqual([]);
  });

  it('the same bearer refused again → authorization_unavailable, with no further cycle', async () => {
    const h = client([refused], { kind: 'granted', grant: BOUND });
    await expect(h.c.status()).rejects.toBeInstanceOf(GrantRevoked);
    await expect(h.c.status()).rejects.toBeInstanceOf(GrantRevoked);
    expect(h.recover).toHaveBeenCalledTimes(1);
    expect(h.calls).toHaveLength(2);
    expect(h.ended).toEqual(['authorization_unavailable']);
    expect(h.forgetCredential).not.toHaveBeenCalled();
    expect(currentGrant()).toBeNull();
    expect(takeAuthorizationEnd()).toBe('authorization_unavailable');
  });

  it.each([
    ['authorization_revoked', true],
    ['authorization_expired', true],
    ['daemon_restarted', true],
  ] as const)('a verified %s ends with that state and forgets the key', async (state, forgets) => {
    const h = client([refused], { kind: 'ended', state });
    await expect(h.c.status()).rejects.toBeInstanceOf(GrantRevoked);
    expect(h.ended).toEqual([state]);
    expect(h.forgetCredential).toHaveBeenCalledTimes(forgets ? 1 : 0);
  });

  it('an unreachable proof keeps the key (daemon_unreachable)', async () => {
    const h = client([refused], { kind: 'unreachable', detail: 'network' });
    await expect(h.c.status()).rejects.toBeInstanceOf(GrantRevoked);
    expect(h.ended).toEqual(['daemon_unreachable']);
    expect(h.forgetCredential).not.toHaveBeenCalled();
  });

  it('concurrent 401s share one proof', async () => {
    let resolve!: (value: RecoveryOutcome) => void;
    const h = client([refused, refused, ok, ok], () => new Promise(r => (resolve = r)));
    const a = h.c.status();
    const b = h.c.launchContexts();
    await new Promise(r => setTimeout(r, 0));
    resolve({ kind: 'granted', grant: BOUND });
    await Promise.all([a, b]);
    expect(h.recover).toHaveBeenCalledTimes(1);
  });
});

describe('socket 1008 reasons', () => {
  it('maps exactly the daemon close reasons', () => {
    expect(endStateForCloseReason('grant revoked')).toBe('authorization_revoked');
    expect(endStateForCloseReason('grant expired')).toBe('authorization_expired');
    expect(endStateForCloseReason('origin removed')).toBe('origin_removed');
    expect(endStateForCloseReason('listener closed')).toBe('listener_closed');
    expect(endStateForCloseReason('daemon stopping')).toBe('daemon_stopped');
    expect(endStateForCloseReason('')).toBeNull();
    expect(endStateForCloseReason(undefined)).toBeNull();
  });

  it('daemon stopping ends without proof and keeps the key; a revocation forgets it', async () => {
    const stopping = client([ok], { kind: 'granted', grant: BOUND });
    expect(await stopping.c.authorizationLost('daemon stopping')).toBe(false);
    expect(stopping.ended).toEqual(['daemon_stopped']);
    expect(stopping.recover).not.toHaveBeenCalled();
    expect(stopping.forgetCredential).not.toHaveBeenCalled();
    const revoked = client([ok], { kind: 'granted', grant: BOUND });
    revoked.c.markRevoked('grant revoked');
    expect(revoked.ended).toEqual(['authorization_revoked']);
    expect(revoked.forgetCredential).toHaveBeenCalledWith(BOUND.reauth!.credentialId);
  });

  it('a sibling socket\'s explicit reason wins over an unexplained close (no proof against a stopping daemon)', async () => {
    const h = client([ok], { kind: 'ended', state: 'listener_changed' });
    const unexplained = h.c.authorizationLost(undefined);
    h.c.markRevoked('daemon stopping');
    expect(await unexplained).toBe(false);
    expect(h.ended).toEqual(['daemon_stopped']);
    expect(h.recover).not.toHaveBeenCalled();
  });

  it('an unexplained policy close disproven by proof is not an ending', async () => {
    const h = client([ok], { kind: 'granted', grant: BOUND });
    expect(await h.c.authorizationLost('')).toBe(true);
    expect(h.ended).toEqual([]);
    expect(h.c.isRevoked).toBe(false);
  });

  it('the first ending wins', async () => {
    const h = client([ok], { kind: 'granted', grant: BOUND });
    h.c.markRevoked('grant expired');
    h.c.markRevoked('grant revoked');
    expect(h.ended).toEqual(['authorization_expired']);
    expect(h.c.endState).toBe('authorization_expired');
  });
});

describe('throttled classification (SR-08)', () => {
  it('a socket ticket request waits out Retry-After and then gets its ticket; user requests are not resent', async () => {
    const outcomes: RecoveryOutcome[] = [{ kind: 'rate_limited', retryAfterSec: 4 }, { kind: 'granted', grant: BOUND }];
    let index = 0;
    const ticket = { status: 201, body: { ticket: `awt_${'T'.repeat(43)}`, expires_at: '2999-01-01T00:00:00Z' } };
    const { calls, transport } = fakeFetch(() => (index++ === 0 ? refused : ticket));
    const sleeps: number[] = [];
    setGrant(BOUND);
    const c = new DaemonClient(BOUND, transport, { recover: async () => outcomes.shift()!, forgetCredential: vi.fn(), unexplainedCloseDelayMs: 0, sleep: async ms => void sleeps.push(ms) });
    await expect(c.ticket('frontend', 'ins_0123456789abcdef0123456789abcdef')).resolves.toMatch(/^awt_/);
    expect(sleeps).toEqual([4000]);
    expect(calls).toHaveLength(2);
    expect(c.endState).toBeNull();
  });
});
