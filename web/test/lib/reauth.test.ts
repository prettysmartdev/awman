import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { DaemonClient, MAX_THROTTLED_PROOFS, MAX_THROTTLE_WAIT_SEC } from '@/lib/client/daemonClient';
import { DaemonError } from '@/lib/client/errors';
import { clearGrant, grantFromWire, setGrant, takeAuthorizationEnd } from '@/lib/client/grant';
import { decodeBase64Url, encodeBase64Url, isBase64UrlOf } from '@/lib/client/base64url';
import { CredentialStore, memoryBackend, type CredentialRecord } from '@/lib/client/credentialStore';
import { HttpTransport } from '@/lib/client/http';
import { isRecoverySuppressed, prove, recoverCredential, recoverGrant, resetRecoveryStateForTests, signChallenge, transcript, validateChallenge, type ChallengeExpectation } from '@/lib/client/reauth';
import type { BrowserReauthChallenge } from '@/lib/client/types';

// Shared Rust/web vector (reauth-daemon step): verified by
// engine::auth::browser_reauth::fixed_vector on the daemon side.
const VECTOR = {
  fields: {
    challenge_id: 'qAfQayBtwabr8uogVAKDZjLHhhbANbeLcIUHfFcUU7o',
    client_nonce: 'lT22AeEkFXHbjl9ayZ4NUdhV9Ct_5HWy7qvS1NhDhv8',
    credential_id: 'ZzrsPJybUwDvrUxbX4oQ93sNK5BaprYgSrVl06tqryU',
    daemon_epoch: 'Gs9c1yhW-SA6P6HLQUsNew',
    expires_at: '2026-11-12T12:00:30.000Z',
    listener: 'loopback',
    listener_epoch: 'C0tOZXEBuALBXtZAsgmDCg',
    nonce: 'WhIdRfvB9yibe_VD8FBpX6RufKCR0k7l5v9lxpMbauE',
    origin: 'http://127.0.0.1:3000',
  },
  public_key: 'BE-_pawjfdRueGK4URlmiPBsnIdvWUOwI1GC5jaMB5ts7WAKNul0gXeRaeUm_5y30mfYPFFKj-CHgeaqkKwSAY4',
  signature: '_N4tPjRQTMAIX4RBxnOGA6gg2T0-teT479XJGYwHRN4MRIfILPzAdo4I_PxmXp-pUu0TSb4oPyqQ5d_9_eyo0A',
  transcript_hex:
    '61776d616e2e62726f777365722e7265617574682e76310000000004504f53540000001b2f76312f62726f777365722d7265617574682f636f6d706c6574650000002b5a7a7273504a7962557744767255786258346f513933734e4b354261707259675372566c303674717279550000001647733963317968572d5341365036484c5155734e6577000000164330744f5a58454275414c4258745a4173676d44436700000015687474703a2f2f3132372e302e302e313a33303030000000086c6f6f706261636b0000002b71416651617942747761627238756f6756414b445a6a4c48686862414e62654c634955486646635555376f0000002b576849645266764239796962655f56443846427058365275664b4352306b376c3576396c78704d626175450000002b6c5432324165456b465848626a6c3961795a344e556468563943745f3548577937717653314e684468763800000018323032362d31312d31325431323a30303a33302e3030305a',
};
const CHALLENGE: BrowserReauthChallenge = { version: 1, ...VECTOR.fields };
const hex = (bytes: Uint8Array) => Buffer.from(bytes).toString('hex');
const ORIGIN = 'http://127.0.0.1:3000';
const BASE = 'http://127.0.0.1:9877';
const EPOCH_D = 'Gs9c1yhW-SA6P6HLQUsNew';
const EPOCH_L = 'C0tOZXEBuALBXtZAsgmDCg';

describe('base64url', () => {
  it('is canonical, unpadded and URL-safe', () => {
    const bytes = new Uint8Array([0xfb, 0xff, 0xfe, 0x00, 0x01]);
    expect(encodeBase64Url(bytes)).toBe('-__-AAE');
    expect(decodeBase64Url('-__-AAE')).toEqual(bytes);
    for (const bad of ['-__-AAE=', '+//+AAE', 'AAF', 'A', ' AAA', 'AB']) expect(decodeBase64Url(bad), bad).toBeNull();
    expect(isBase64UrlOf(VECTOR.fields.credential_id, 32)).toBe(true);
    expect(isBase64UrlOf(VECTOR.fields.daemon_epoch, 16)).toBe(true);
    expect(isBase64UrlOf(VECTOR.fields.daemon_epoch, 32)).toBe(false);
  });
});

describe('transcript and signature (C3)', () => {
  it('matches the shared daemon vector byte for byte', () => {
    expect(hex(transcript(CHALLENGE))).toBe(VECTOR.transcript_hex);
    // The example from C3: "POST" contributes 00 00 00 04 50 4f 53 54.
    expect(VECTOR.transcript_hex).toContain('00' + '000004504f5354');
  });

  it('verifies the daemon-signed raw r||s vector with WebCrypto (no DER, one hash)', async () => {
    const key = await crypto.subtle.importKey('raw', decodeBase64Url(VECTOR.public_key)!, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
    const signature = decodeBase64Url(VECTOR.signature)!;
    expect(signature.length).toBe(64);
    expect(VECTOR.signature).toHaveLength(86);
    expect(await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, signature, transcript(CHALLENGE))).toBe(true);
    const tampered = transcript({ ...CHALLENGE, listener: 'other' });
    expect(await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, signature, tampered)).toBe(false);
  });

  it('length-prefixes non-ASCII values by UTF-8 bytes', () => {
    const bytes = transcript({ ...CHALLENGE, listener: 'lö' });
    expect(hex(bytes)).toContain('000000036cc3b6');
  });

  it('signs as 86-char base64url that verifies under the exported public key', async () => {
    const pair = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']);
    const signature = await signChallenge(pair.privateKey, CHALLENGE);
    expect(signature).toMatch(/^[A-Za-z0-9_-]{86}$/);
    expect(await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, pair.publicKey, decodeBase64Url(signature)!, transcript(CHALLENGE))).toBe(true);
  });
});

describe('challenge validation before signing', () => {
  const expected: ChallengeExpectation = {
    credentialId: CHALLENGE.credential_id,
    daemonEpoch: CHALLENGE.daemon_epoch,
    listenerEpoch: CHALLENGE.listener_epoch,
    origin: CHALLENGE.origin,
    listener: CHALLENGE.listener,
    clientNonce: CHALLENGE.client_nonce,
  };
  it('accepts exactly the expected response', () => {
    expect(validateChallenge({ ...CHALLENGE }, expected)).toEqual(CHALLENGE);
  });
  it.each([
    ['extra field', { ...CHALLENGE, grant: 'x' }],
    ['missing field', (({ nonce: _n, ...rest }) => rest)(CHALLENGE)],
    ['version', { ...CHALLENGE, version: 2 }],
    ['credential substitution', { ...CHALLENGE, credential_id: 'A'.repeat(43) }],
    ['daemon epoch substitution', { ...CHALLENGE, daemon_epoch: 'A'.repeat(22) }],
    ['listener epoch substitution', { ...CHALLENGE, listener_epoch: 'A'.repeat(22) }],
    ['listener substitution', { ...CHALLENGE, listener: 'other' }],
    ['client nonce substitution', { ...CHALLENGE, client_nonce: 'A'.repeat(43) }],
    ['short nonce', { ...CHALLENGE, nonce: 'AAAA' }],
    ['padded challenge id', { ...CHALLENGE, challenge_id: CHALLENGE.challenge_id + '=' }],
    ['reformatted expiry', { ...CHALLENGE, expires_at: '2026-11-12T12:00:30Z' }],
  ])('refuses %s', (_name, body) => {
    expect(() => validateChallenge(body, expected)).toThrow();
  });
  it('reports a different page origin distinctly', () => {
    expect(() => validateChallenge({ ...CHALLENGE, origin: 'http://localhost:3000' }, expected)).toThrow(/different site origin/);
  });
});

// A fake daemon for challenge/complete with a real WebCrypto signature check.
async function setup(options: { grantId?: string | null; linkExpiresAt?: string | null; before?: (path: string) => Response | null } = {}) {
  const backend = memoryBackend();
  const store = new CredentialStore(backend, { origin: ORIGIN, daemonBase: BASE });
  const candidate = (await store.createCandidate({ listener: 'loopback', daemonEpoch: EPOCH_D, listenerEpoch: EPOCH_L }))!;
  const record: CredentialRecord = (await store.update(candidate.record.credentialId, { grantId: options.grantId === undefined ? 'brw_0123' : options.grantId, linkExpiresAt: options.linkExpiresAt ?? null }))!;
  const verifier = await crypto.subtle.importKey('raw', decodeBase64Url(candidate.publicKey)!, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
  const issued = new Map<string, BrowserReauthChallenge>();
  const grant = (reauth: Record<string, unknown> = { version: 1, credential_id: record.credentialId, daemon_epoch: EPOCH_D, listener_epoch: EPOCH_L }) => ({
    id: 'brw_0123',
    token: `awb_${'G'.repeat(43)}`,
    scope: 'engine',
    origin: ORIGIN,
    listener: 'loopback',
    expires_at: '2999-01-01T00:00:00.000Z',
    reauth,
  });
  let challengeOverride: ((body: BrowserReauthChallenge) => unknown) | null = null;
  let grantOverride: unknown = null;
  const verified: boolean[] = [];
  const calls: string[] = [];
  const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
  const transport = new HttpTransport(async (url, init) => {
    const path = url.slice(BASE.length);
    calls.push(path);
    const intercepted = options.before?.(path);
    if (intercepted) return intercepted;
    expect((init.headers as Record<string, string>).Authorization).toBeUndefined();
    const body = JSON.parse(String(init.body)) as Record<string, string>;
    if (path === '/v1/browser-reauth/challenge') {
      const challenge: BrowserReauthChallenge = {
        version: 1,
        credential_id: body.credential_id!,
        daemon_epoch: body.daemon_epoch!,
        listener_epoch: body.listener_epoch!,
        origin: ORIGIN,
        listener: 'loopback',
        challenge_id: encodeBase64Url(crypto.getRandomValues(new Uint8Array(32))),
        nonce: encodeBase64Url(crypto.getRandomValues(new Uint8Array(32))),
        client_nonce: body.client_nonce!,
        expires_at: '2999-01-01T00:00:30.000Z',
      };
      issued.set(challenge.challenge_id, challenge);
      return json(201, challengeOverride ? challengeOverride(challenge) : challenge);
    }
    const challenge = issued.get(body.challenge_id!);
    issued.delete(body.challenge_id!); // one use
    const ok = !!challenge && (await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, verifier, decodeBase64Url(body.signature!)!, transcript(challenge)));
    verified.push(ok);
    return ok ? json(200, { grant: grantOverride ?? grant() }) : json(401, { code: 'reauth_failed', error: 'Browser reauthorization proof was not accepted.' });
  }, BASE);
  return {
    store,
    backend,
    record,
    calls,
    verified,
    transport,
    setChallenge: (fn: (body: BrowserReauthChallenge) => unknown) => (challengeOverride = fn),
    setGrant: (value: unknown) => (grantOverride = value),
    grant,
  };
}

const always = (status: number, code: string) =>
  new HttpTransport(async () => new Response(JSON.stringify({ code, error: code }), { status, headers: { 'Retry-After': '7' } }), BASE);

beforeEach(() => resetRecoveryStateForTests());

describe('proof flow', () => {
  it('recovers the same grant without Authorization and records public metadata only', async () => {
    const h = await setup({ grantId: null });
    const outcome = await recoverCredential(h.record, { store: h.store, transport: h.transport, origin: ORIGIN });
    expect(outcome.kind).toBe('granted');
    if (outcome.kind !== 'granted') return;
    expect(outcome.grant).toMatchObject({ id: 'brw_0123', token: `awb_${'G'.repeat(43)}`, expiresAt: '2999-01-01T00:00:00.000Z' });
    expect(h.verified).toEqual([true]);
    const stored = h.backend.data.get(h.record.credentialId) as CredentialRecord;
    expect(stored.grantId).toBe('brw_0123');
    expect(JSON.stringify({ ...stored, privateKey: null })).not.toMatch(/awb_|awt_|signature/);
  });

  it('retries one fresh challenge on reauth_failed, then deletes a dead binding', async () => {
    const h = await setup();
    // Wrong key at the daemon: each completion consumes its challenge and fails.
    const other = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']);
    const prove = { ...h.record, privateKey: other.privateKey };
    const outcome = await recoverCredential(prove, { store: { ...h.store, keyMatches: async () => true, remove: (id: string) => h.store.remove(id), update: h.store.update.bind(h.store) } as unknown as CredentialStore, transport: h.transport, origin: ORIGIN });
    expect(outcome).toEqual({ kind: 'failed' });
    // Exactly two fresh challenges, never a resent complete body.
    expect(h.calls).toEqual(['/v1/browser-reauth/challenge', '/v1/browser-reauth/complete', '/v1/browser-reauth/challenge', '/v1/browser-reauth/complete']);
    expect(h.verified).toEqual([false, false]);
    expect(h.backend.data.has(h.record.credentialId)).toBe(false);
  });

  it('keeps an unapproved candidate inside its link deadline after two failures', async () => {
    const h = await setup({ grantId: null, linkExpiresAt: '2999-01-01T00:05:00.000Z' });
    expect(await recoverCredential(h.record, { store: h.store, transport: always(401, 'reauth_failed'), origin: ORIGIN })).toEqual({ kind: 'failed' });
    expect(h.backend.data.has(h.record.credentialId)).toBe(true);
  });

  it.each([
    [409, 'daemon_restarted', { kind: 'ended', state: 'daemon_restarted' }, false],
    [409, 'listener_changed', { kind: 'ended', state: 'listener_changed' }, false],
    [403, 'authorization_expired', { kind: 'ended', state: 'authorization_expired' }, false],
    [403, 'authorization_revoked', { kind: 'ended', state: 'authorization_revoked' }, false],
    [429, 'rate_limited', { kind: 'rate_limited', retryAfterSec: 7 }, true],
    [400, 'invalid_request', { kind: 'invalid' }, true],
  ] as const)('maps %i %s and applies cleanup', async (status, code, expected, kept) => {
    const h = await setup();
    expect(await recoverCredential(h.record, { store: h.store, transport: always(status, code), origin: ORIGIN })).toEqual(expected);
    expect(h.backend.data.has(h.record.credentialId)).toBe(kept);
  });

  it('never discards the key on a network failure', async () => {
    const h = await setup();
    const transport = new HttpTransport(async () => {
      throw new TypeError('Failed to fetch');
    }, BASE);
    expect(await recoverCredential(h.record, { store: h.store, transport, origin: ORIGIN })).toEqual({ kind: 'unreachable', detail: 'network' });
    expect(h.backend.data.has(h.record.credentialId)).toBe(true);
  });

  it('does not sign a challenge naming another origin, nonce or listener', async () => {
    for (const change of [{ origin: 'http://localhost:3000' }, { client_nonce: 'B'.repeat(43) }, { listener: 'geometry' }, { nonce: 'short' }]) {
      const h = await setup();
      h.setChallenge(body => ({ ...body, ...change }));
      const outcome = await prove(h.record, { transport: h.transport, origin: ORIGIN });
      expect(outcome.kind === 'ended' || outcome.kind === 'invalid').toBe(true);
      if ('origin' in change) expect(outcome).toEqual({ kind: 'ended', state: 'origin_mismatch' });
      expect(h.calls).not.toContain('/v1/browser-reauth/complete');
    }
  });

  it('refuses a released grant with another binding, origin or id', async () => {
    for (const variant of ['binding', 'origin', 'id'] as const) {
      const h = await setup();
      const good = h.grant();
      h.setGrant(
        variant === 'binding'
          ? h.grant({ version: 1, credential_id: 'A'.repeat(43), daemon_epoch: EPOCH_D, listener_epoch: EPOCH_L })
          : variant === 'origin'
            ? { ...good, origin: 'http://evil.test' }
            : { ...good, id: 'brw_other' },
      );
      const outcome = await prove(h.record, { transport: h.transport, origin: ORIGIN });
      expect(outcome.kind).not.toBe('granted');
    }
  });

  it('a record for another page origin is never sent', async () => {
    const h = await setup();
    expect(await prove(h.record, { transport: h.transport, origin: 'http://localhost:3000' })).toEqual({ kind: 'ended', state: 'origin_mismatch' });
    expect(h.calls).toHaveLength(0);
  });

  it('deduplicates concurrent recovery of one credential within the document', async () => {
    const h = await setup();
    const [a, b] = await Promise.all([
      recoverCredential(h.record, { store: h.store, transport: h.transport, origin: ORIGIN }),
      recoverCredential(h.record, { store: h.store, transport: h.transport, origin: ORIGIN }),
    ]);
    expect(a).toBe(b);
    expect(h.verified).toEqual([true]);
  });

  it('drops a record whose private key does not match its public key (swapped key)', async () => {
    const h = await setup();
    const other = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']);
    const swapped = { ...h.record, privateKey: other.privateKey };
    expect(await recoverCredential(swapped, { store: h.store, transport: h.transport, origin: ORIGIN })).toEqual({ kind: 'ended', state: 'authorization_unavailable' });
    expect(h.calls).toHaveLength(0);
    expect(h.backend.data.has(h.record.credentialId)).toBe(false);
  });
});


// SR-08: a 429 while classifying an operational 401 or an unexplained 1008 is
// temporary. Real proof code (recoverGrant → prove → WebCrypto signature) runs
// against the fake daemon; only time is controlled.
describe('throttled proof during authorization-loss classification (SR-08)', () => {
  const reply = (status: number, body: unknown, headers: Record<string, string> = {}) =>
    new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
  afterEach(() => {
    clearGrant();
    takeAuthorizationEnd();
  });

  async function throttled(retryAfter: string, throttledChallenges: number) {
    let challenges = 0;
    let statuses = 0;
    const h = await setup({
      before: path => {
        if (path === '/v1/status') return ++statuses === 1 ? reply(401, { code: 'browser_grant_invalid', error: 'invalid' }) : reply(200, { pid: 1 });
        if (path === '/v1/browser-reauth/challenge' && ++challenges <= throttledChallenges) return reply(429, { code: 'rate_limited', error: 'slow down' }, { 'Retry-After': retryAfter });
        return null;
      },
    });
    const grant = grantFromWire(h.grant() as unknown as Parameters<typeof grantFromWire>[0]);
    setGrant(grant);
    const sleeps: number[] = [];
    let wake: (() => void) | null = null;
    const sleep = vi.fn((ms: number) => {
      sleeps.push(ms);
      return new Promise<void>(resolve => (wake = resolve));
    });
    const forgetCredential = vi.fn(async (id: string) => void (await h.store.remove(id)));
    const client = new DaemonClient(grant, h.transport, {
      recover: value => recoverGrant(value, { store: h.store, transport: h.transport, origin: ORIGIN }),
      forgetCredential,
      unexplainedCloseDelayMs: 0,
      sleep,
    });
    const ended: string[] = [];
    const notices: number[] = [];
    client.onAuthEnded(state => ended.push(state));
    client.onRecoveryThrottled(sec => notices.push(sec));
    const settle = async () => {
      for (let i = 0; i < 20; i++) await new Promise(resolve => setTimeout(resolve, 0));
    };
    return { h, client, grant, sleeps, sleep, forgetCredential, ended, notices, settle, wakeUp: () => wake?.(), statusCalls: () => statuses, challengeCalls: () => challenges };
  }

  it('401 → proof 429 → Retry-After → successful fresh proof in the same document; nothing replayed', async () => {
    const t = await throttled('3', 1);
    const error = await t.client.status().catch(e => e);
    // The triggering request fails visibly as temporary; it is not resent later.
    expect(error).toBeInstanceOf(DaemonError);
    expect(error).toMatchObject({ status: 429, code: 'rate_limited', retryAfterSec: 3 });
    expect(t.notices).toEqual([3]);
    expect(t.sleeps).toEqual([3000]);
    expect(t.client.recoveryThrottledUntil).not.toBeNull();
    // Temporary: not ended, not suppressed, key kept.
    expect(t.client.endState).toBeNull();
    expect(t.client.isRevoked).toBe(false);
    expect(isRecoverySuppressed(t.h.record.credentialId)).toBe(false);
    expect(t.forgetCredential).not.toHaveBeenCalled();
    expect(t.h.backend.data.has(t.h.record.credentialId)).toBe(true);
    // Retry-After elapses: one fresh challenge and a verified proof of the same grant.
    t.wakeUp();
    await t.settle();
    expect(t.challengeCalls()).toBe(2);
    expect(t.h.verified).toEqual([true]);
    expect(t.client.recoveryThrottledUntil).toBeNull();
    expect(t.ended).toEqual([]);
    expect(t.statusCalls()).toBe(1); // no automatic replay of the failed request
    // A new user request works with the same in-memory bearer.
    await expect(t.client.status()).resolves.toEqual({ pid: 1 });
    expect(t.statusCalls()).toBe(2);
    expect(t.h.calls.filter(path => path === '/v1/browser-links')).toEqual([]);
  });

  it('unexplained 1008 → proof 429 → Retry-After → fresh proof: the socket is told to reconnect normally', async () => {
    const t = await throttled('2', 1);
    const verdict = t.client.authorizationLost('');
    await t.settle();
    expect(t.notices).toEqual([2]);
    expect(t.client.isRevoked).toBe(false);
    expect(isRecoverySuppressed(t.h.record.credentialId)).toBe(false);
    t.wakeUp();
    expect(await verdict).toBe(true);
    expect(t.challengeCalls()).toBe(2);
    expect(t.ended).toEqual([]);
    expect(t.h.backend.data.has(t.h.record.credentialId)).toBe(true);
  });

  it('bounds the schedule: clamped waits, then a non-destructive daemon_unreachable (no suppression, key kept)', async () => {
    const t = await throttled('99999', 1000);
    const verdict = t.client.authorizationLost('');
    for (let i = 1; i < MAX_THROTTLED_PROOFS; i++) {
      await t.settle();
      t.wakeUp();
    }
    expect(await verdict).toBe(false);
    expect(t.challengeCalls()).toBe(MAX_THROTTLED_PROOFS);
    expect(t.sleeps).toEqual(Array(MAX_THROTTLED_PROOFS - 1).fill(MAX_THROTTLE_WAIT_SEC * 1000));
    expect(t.ended).toEqual(['daemon_unreachable']);
    expect(isRecoverySuppressed(t.h.record.credentialId)).toBe(false);
    expect(t.forgetCredential).not.toHaveBeenCalled();
    expect(t.h.backend.data.has(t.h.record.credentialId)).toBe(true);
  });

  it('an explicit ending reason during the wait wins; the scheduled proof never runs', async () => {
    const t = await throttled('5', 1);
    const verdict = t.client.authorizationLost('');
    await t.settle();
    t.client.markRevoked('grant revoked');
    t.wakeUp();
    expect(await verdict).toBe(false);
    expect(t.challengeCalls()).toBe(1);
    expect(t.ended).toEqual(['authorization_revoked']);
  });
});
