// The browser grant lives ONLY in this module's memory (design §2.5, §5.7):
// never in a URL, cookie, localStorage, sessionStorage, IndexedDB, Cache
// Storage or log. A reload or browser restart forgets it; the user re-links.
import type { BrowserGrantWire } from './types';

export interface Grant {
  id: string;
  token: string;
  scope: 'engine';
  origin: string;
  listener: string;
  expiresAt: string;
}

let current: Grant | null = null;
const listeners = new Set<(grant: Grant | null) => void>();

export function grantFromWire(wire: BrowserGrantWire): Grant {
  if (!/^awb_[A-Za-z0-9_-]{16,128}$/.test(wire.token)) throw new Error('malformed browser grant');
  return { id: wire.id, token: wire.token, scope: wire.scope, origin: wire.origin, listener: wire.listener, expiresAt: wire.expires_at };
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
  for (const listener of [...listeners]) listener(grant);
}

export function clearGrant(): void {
  setGrant(null);
}

export function onGrantChange(listener: (grant: Grant | null) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}
