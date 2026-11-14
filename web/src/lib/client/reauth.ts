// Saved-key browser reauthorization (WI 0160 C3, C4): challenge → exact typed
// transcript → raw P-256 signature → the SAME grant (id, bearer, expiry).
//
// - Challenge/complete carry no Authorization and use the fixed daemon base
//   through HttpTransport; a stored daemonBase is never a destination.
// - Every challenge response field is validated against the selected record,
//   the trusted page origin and this request's fresh client nonce BEFORE
//   signing; additional or missing fields are refused.
// - Signs only the length-prefixed transcript, never server-supplied bytes.
// - One recovery per credential per document at a time; other tabs prove
//   independently. One fresh-challenge retry on `reauth_failed`. A complete
//   body is never resent (it is consumed even on failure).
// - Proofs, signatures and bearers never reach logs, storage or messages.
import { decodeBase64Url, encodeBase64Url, isBase64UrlOf, randomBase64Url } from './base64url';
import { credentialStore, type CredentialRecord, type CredentialStore } from './credentialStore';
import { DaemonError, DaemonUnreachable, ProtocolError } from './errors';
import { grantFromWire, sameBinding, type Grant } from './grant';
import { HttpTransport } from './http';
import type { AuthEndState } from './recovery';
import type { BrowserReauthChallenge, BrowserReauthChallengeRequest, BrowserReauthComplete, BrowserReauthCompleteRequest } from './types';

export const TRANSCRIPT_DOMAIN = 'awman.browser.reauth.v1';
export const COMPLETE_PATH = '/v1/browser-reauth/complete';
export const CHALLENGE_PATH = '/v1/browser-reauth/challenge';
const CHALLENGE_KEYS = ['version', 'credential_id', 'daemon_epoch', 'listener_epoch', 'origin', 'listener', 'challenge_id', 'nonce', 'client_nonce', 'expires_at'];
const EXPIRES_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;

/**
 * `awman.browser.reauth.v1 || 0x00`, then eleven `u32be(len) || utf8` strings:
 * POST, the complete path and the challenge fields in C3 order, verbatim.
 */
export function transcript(challenge: BrowserReauthChallenge): Uint8Array {
  const encoder = new TextEncoder();
  const parts = [
    'POST',
    COMPLETE_PATH,
    challenge.credential_id,
    challenge.daemon_epoch,
    challenge.listener_epoch,
    challenge.origin,
    challenge.listener,
    challenge.challenge_id,
    challenge.nonce,
    challenge.client_nonce,
    challenge.expires_at,
  ].map(part => encoder.encode(part));
  const domain = encoder.encode(TRANSCRIPT_DOMAIN);
  const out = new Uint8Array(domain.length + 1 + parts.reduce((sum, part) => sum + 4 + part.length, 0));
  const view = new DataView(out.buffer);
  out.set(domain, 0);
  let offset = domain.length + 1; // the zero byte is already 0
  for (const part of parts) {
    view.setUint32(offset, part.length, false);
    out.set(part, offset + 4);
    offset += 4 + part.length;
  }
  return out;
}

export interface ChallengeExpectation {
  credentialId: string;
  daemonEpoch: string;
  listenerEpoch: string;
  /** Trusted `location.origin`. */
  origin: string;
  listener: string;
  clientNonce: string;
}

/** The challenge names another page origin than the trusted one: never sign. */
export class OriginMismatch extends Error {
  override readonly name = 'OriginMismatch';
  constructor() {
    super('The saved authorization belongs to a different site origin.');
  }
}

/** Validate a challenge 201 completely before it may be signed. */
export function validateChallenge(body: unknown, expected: ChallengeExpectation): BrowserReauthChallenge {
  if (typeof body !== 'object' || body === null || Array.isArray(body)) throw new ProtocolError('malformed reauthorization challenge');
  const value = body as Record<string, unknown>;
  const keys = Object.keys(value);
  if (keys.length !== CHALLENGE_KEYS.length || !CHALLENGE_KEYS.every(key => keys.includes(key))) throw new ProtocolError('unexpected reauthorization challenge fields');
  if (value.version !== 1) throw new ProtocolError('unsupported reauthorization challenge version');
  if (typeof value.origin !== 'string' || value.origin !== expected.origin) throw new OriginMismatch();
  const ok =
    value.credential_id === expected.credentialId &&
    value.daemon_epoch === expected.daemonEpoch &&
    value.listener_epoch === expected.listenerEpoch &&
    value.listener === expected.listener &&
    value.client_nonce === expected.clientNonce &&
    isBase64UrlOf(value.challenge_id, 32) &&
    isBase64UrlOf(value.nonce, 32) &&
    typeof value.expires_at === 'string' &&
    EXPIRES_RE.test(value.expires_at) &&
    Number.isFinite(Date.parse(value.expires_at));
  if (!ok) throw new ProtocolError('reauthorization challenge does not match this credential');
  return value as unknown as BrowserReauthChallenge;
}

/** ECDSA P-256 / SHA-256 over the transcript; raw r||s as 86-char base64url. */
export async function signChallenge(privateKey: CryptoKey, challenge: BrowserReauthChallenge): Promise<string> {
  const signature = new Uint8Array(await crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, privateKey, transcript(challenge)));
  if (signature.length !== 64) throw new ProtocolError('unexpected signature length');
  return encodeBase64Url(signature);
}

export type ProofOutcome =
  | { kind: 'granted'; grant: Grant }
  /** Verified, specific ending (409 generation / 403 after valid proof / local origin mismatch). */
  | { kind: 'ended'; state: AuthEndState }
  /** Two consecutive `reauth_failed` on fresh challenges: the daemon holds no usable binding. */
  | { kind: 'failed' }
  | { kind: 'rate_limited'; retryAfterSec: number }
  | { kind: 'unreachable'; detail: 'network' | 'blocked_by_browser' | 'timeout' }
  /** Protocol/validation failure (400, malformed or mismatching response). Never an automatic link. */
  | { kind: 'invalid' };

export interface ProveDeps {
  transport?: HttpTransport;
  /** Trusted page origin (default `location.origin`). */
  origin?: string;
  signal?: AbortSignal;
}

function pageOrigin(): string {
  return typeof location !== 'undefined' && typeof location.origin === 'string' ? location.origin : '';
}

function errorOutcome(error: unknown): ProofOutcome | 'retry' {
  if (error instanceof OriginMismatch) return { kind: 'ended', state: 'origin_mismatch' };
  if (error instanceof DaemonUnreachable) return { kind: 'unreachable', detail: error.detail };
  if (error instanceof DaemonError) {
    switch (error.code) {
      case 'daemon_restarted':
        return { kind: 'ended', state: 'daemon_restarted' };
      case 'listener_changed':
        return { kind: 'ended', state: 'listener_changed' };
      case 'authorization_expired':
        return { kind: 'ended', state: 'authorization_expired' };
      case 'authorization_revoked':
        return { kind: 'ended', state: 'authorization_revoked' };
      case 'reauth_failed':
        return 'retry';
      case 'rate_limited':
        return { kind: 'rate_limited', retryAfterSec: Math.max(1, Math.ceil(error.retryAfterSec ?? 30)) };
      default:
        return { kind: 'invalid' };
    }
  }
  return { kind: 'invalid' };
}

async function proveOnce(record: CredentialRecord, transport: HttpTransport, origin: string, signal?: AbortSignal): Promise<Grant> {
  const clientNonce = randomBase64Url(32);
  const challengeRequest: BrowserReauthChallengeRequest = {
    version: 1,
    credential_id: record.credentialId,
    daemon_epoch: record.daemonEpoch,
    listener_epoch: record.listenerEpoch,
    client_nonce: clientNonce,
  };
  const { body } = await transport.json<unknown>(CHALLENGE_PATH, { method: 'POST', body: challengeRequest, signal });
  const challenge = validateChallenge(body, {
    credentialId: record.credentialId,
    daemonEpoch: record.daemonEpoch,
    listenerEpoch: record.listenerEpoch,
    origin,
    listener: record.listener,
    clientNonce,
  });
  const complete: BrowserReauthCompleteRequest = { version: 1, challenge_id: challenge.challenge_id, signature: await signChallenge(record.privateKey, challenge) };
  const { body: released } = await transport.json<BrowserReauthComplete>(COMPLETE_PATH, { method: 'POST', body: complete, signal });
  const grant = grantFromWire((released as Partial<BrowserReauthComplete> | null)?.grant as BrowserReauthComplete['grant']);
  // Install only a grant that is exactly this credential's authorization.
  const binding = { credentialId: record.credentialId, daemonEpoch: record.daemonEpoch, listenerEpoch: record.listenerEpoch };
  if (!sameBinding(grant.reauth, binding)) throw new ProtocolError('recovered grant does not match the credential binding');
  if (grant.origin !== origin) throw new OriginMismatch();
  if (grant.listener !== record.listener) throw new ProtocolError('recovered grant names another listener');
  if (record.grantId !== null && grant.id !== record.grantId) throw new ProtocolError('recovered grant id changed');
  if (record.expiresAt !== null && grant.expiresAt !== record.expiresAt) throw new ProtocolError('recovered grant expiry changed');
  return grant;
}

/** Prove possession of `record`'s key: one attempt plus one fresh-challenge retry on reauth_failed. */
export async function prove(record: CredentialRecord, deps: ProveDeps = {}): Promise<ProofOutcome> {
  const transport = deps.transport ?? new HttpTransport();
  const origin = deps.origin ?? pageOrigin();
  if (record.origin !== origin) return { kind: 'ended', state: 'origin_mismatch' };
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      return { kind: 'granted', grant: await proveOnce(record, transport, origin, deps.signal) };
    } catch (error) {
      if (deps.signal?.aborted) throw error;
      const outcome = errorOutcome(error);
      if (outcome !== 'retry') return outcome;
    }
  }
  return { kind: 'failed' };
}

export type RecoveryOutcome = ProofOutcome | { kind: 'no_credential' };

export interface RecoverDeps extends ProveDeps {
  store?: CredentialStore;
  now?: () => number;
}

const inflight = new Map<string, Promise<RecoveryOutcome>>();

/**
 * Recover the grant of one saved record and apply C4 cleanup: verified
 * endings and dead bindings delete the record; timeouts, 429 and network
 * failures never do; an unapproved candidate inside its link deadline is kept.
 * Deduplicated per credential within this document.
 */
export function recoverCredential(record: CredentialRecord, deps: RecoverDeps = {}): Promise<RecoveryOutcome> {
  const running = inflight.get(record.credentialId);
  if (running) return running;
  const promise = runRecovery(record, deps).finally(() => inflight.delete(record.credentialId));
  inflight.set(record.credentialId, promise);
  return promise;
}

async function runRecovery(record: CredentialRecord, deps: RecoverDeps): Promise<RecoveryOutcome> {
  const store = deps.store ?? credentialStore();
  const now = deps.now ?? Date.now;
  if (!(await store.keyMatches(record))) {
    await store.remove(record.credentialId); // swapped or corrupt key: never use it
    return { kind: 'ended', state: 'authorization_unavailable' };
  }
  const outcome = await prove(record, deps);
  switch (outcome.kind) {
    case 'granted':
      if (record.grantId !== outcome.grant.id || record.expiresAt !== outcome.grant.expiresAt) {
        await store.update(record.credentialId, { grantId: outcome.grant.id, expiresAt: outcome.grant.expiresAt });
      }
      break;
    case 'ended':
      if (outcome.state !== 'origin_mismatch') await store.remove(record.credentialId);
      break;
    case 'failed': {
      const pending = record.grantId === null && record.linkExpiresAt !== null && Date.parse(record.linkExpiresAt) > now();
      if (!pending) await store.remove(record.credentialId);
      break;
    }
    default:
      break;
  }
  return outcome;
}

/**
 * Classify the loss of an in-memory key-bound grant (operational 401 or an
 * unexplained 1008) by proving its saved key once. Keyless grants and missing
 * records yield `no_credential`.
 */
export async function recoverGrant(grant: Grant, deps: RecoverDeps = {}): Promise<RecoveryOutcome> {
  const credentialId = grant.reauth?.credentialId;
  if (!credentialId) return { kind: 'no_credential' };
  const store = deps.store ?? credentialStore();
  const { usable } = await store.load();
  const record = usable.find(candidate => candidate.credentialId === credentialId);
  if (!record) return { kind: 'no_credential' };
  return recoverCredential(record, deps);
}

/** Exposed for tests: decode a signature to its raw 64 bytes. */
export function rawSignature(signature: string): Uint8Array | null {
  const bytes = decodeBase64Url(signature);
  return bytes && bytes.length === 64 ? bytes : null;
}

const suppressed = new Set<string>();

/**
 * Stop automatic recovery of a credential in this document: its proof
 * succeeded but the daemon refused the same bearer again (C3), so another
 * cycle cannot help. An explicit new link uses a fresh key instead.
 */
export function suppressRecovery(credentialId: string): void {
  suppressed.add(credentialId);
}

export function isRecoverySuppressed(credentialId: string): boolean {
  return suppressed.has(credentialId);
}

/** Tests only. */
export function resetRecoveryStateForTests(): void {
  suppressed.clear();
  inflight.clear();
}
