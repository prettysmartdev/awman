// The browser grant lives ONLY in this module's memory (design §2.5, §5.7;
// WI 0160 C4): never in a URL, cookie, localStorage, sessionStorage,
// IndexedDB, Cache Storage or log. A reload forgets it. A key-bound grant is
// recovered after reload by proof of the saved signing key
// (credentialStore.ts + reauth.ts), which returns this same grant again.
import { isCredentialId, isEpoch } from './credentialStore';
import type { AuthEndState } from './recovery';
import type { BrowserGrantWire, BrowserReauthBindingWire } from './types';

/** Public binding of a key-bound grant (`grant.reauth`, C2). */
export interface ReauthBinding {
  credentialId: string;
  daemonEpoch: string;
  listenerEpoch: string;
}

export interface Grant {
  id: string;
  token: string;
  scope: 'engine';
  origin: string;
  listener: string;
  expiresAt: string;
  /** Present for key-bound grants only; keyless grants cannot be recovered. */
  reauth?: ReauthBinding;
}

let current: Grant | null = null;
let ended: AuthEndState | null = null;
const listeners = new Set<(grant: Grant | null) => void>();

/** Exactly `{version:1, credential_id, daemon_epoch, listener_epoch}`; null otherwise. */
export function bindingFromWire(wire: unknown): ReauthBinding | null {
  if (typeof wire !== 'object' || wire === null || Array.isArray(wire)) return null;
  const value = wire as Partial<BrowserReauthBindingWire> & Record<string, unknown>;
  if (Object.keys(value).sort().join(',') !== 'credential_id,daemon_epoch,listener_epoch,version') return null;
  if (value.version !== 1 || !isCredentialId(value.credential_id) || !isEpoch(value.daemon_epoch) || !isEpoch(value.listener_epoch)) return null;
  return { credentialId: value.credential_id, daemonEpoch: value.daemon_epoch, listenerEpoch: value.listener_epoch };
}

export function sameBinding(a: ReauthBinding | null | undefined, b: ReauthBinding | null | undefined): boolean {
  return !!a && !!b && a.credentialId === b.credentialId && a.daemonEpoch === b.daemonEpoch && a.listenerEpoch === b.listenerEpoch;
}

export function grantFromWire(wire: BrowserGrantWire): Grant {
  if (typeof wire !== 'object' || wire === null) throw new Error('malformed browser grant');
  if (typeof wire.token !== 'string' || !/^awb_[A-Za-z0-9_-]{16,128}$/.test(wire.token)) throw new Error('malformed browser grant');
  if (typeof wire.id !== 'string' || wire.scope !== 'engine' || typeof wire.origin !== 'string' || typeof wire.listener !== 'string' || typeof wire.expires_at !== 'string') {
    throw new Error('malformed browser grant');
  }
  const grant: Grant = { id: wire.id, token: wire.token, scope: wire.scope, origin: wire.origin, listener: wire.listener, expiresAt: wire.expires_at };
  if (wire.reauth !== undefined) {
    const reauth = bindingFromWire(wire.reauth);
    if (!reauth) throw new Error('malformed browser grant binding');
    grant.reauth = reauth;
  }
  return grant;
}

export function isGrantLive(grant: Grant | null, now: number = Date.now()): grant is Grant {
  if (!grant) return false;
  const expires = Date.parse(grant.expiresAt);
  return Number.isFinite(expires) && expires > now;
}

export function currentGrant(now: number = Date.now()): Grant | null {
  return isGrantLive(current, now) ? current : null;
}

export function setGrant(grant: Grant | null): void {
  current = grant;
  if (grant) ended = null;
  for (const listener of [...listeners]) listener(grant);
}

export function clearGrant(): void {
  setGrant(null);
}

/**
 * The authorization ended for this document with a classified reason. Clears
 * the in-memory bearer; the connect controller reads the reason once to show
 * its exact C7 explanation.
 */
export function endAuthorization(state: AuthEndState): void {
  ended = state;
  setGrant(null);
  ended = state;
}

/** The last classified ending, consumed once by the connect controller. */
export function takeAuthorizationEnd(): AuthEndState | null {
  const state = ended;
  ended = null;
  return state;
}

export function onGrantChange(listener: (grant: Grant | null) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}
