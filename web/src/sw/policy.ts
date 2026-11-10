// Pure service-worker policy (design §5.8), unit-tested in test/lib/sw.test.ts
// and bundled into out/sw.js by scripts/build-sw.mjs.
//
// The service worker caches ONLY the exported app shell (HTML, /_next/static,
// icons, manifest). It never touches daemon traffic: requests to the daemon's
// separate port are a different origin and are not intercepted, so
// daemon responses, snapshots, terminal bytes, grants and commands can never
// enter Cache Storage.

export const CACHE_PREFIX = 'awman-shell-';

export interface PrecacheManifest {
  /** Content hash of every precached file; changes whenever any shell file changes. */
  version: string;
  /** Absolute same-origin paths, e.g. "/", "/_next/static/chunks/x.js". */
  urls: string[];
}

export function cacheNameFor(version: string): string {
  return `${CACHE_PREFIX}${version}`;
}

/** Caches from older shell versions, to delete once the new version activates. */
export function staleCaches(existing: readonly string[], version: string): string[] {
  const keep = cacheNameFor(version);
  return existing.filter(name => name.startsWith(CACHE_PREFIX) && name !== keep);
}

export interface RequestFacts {
  url: string;
  method: string;
  mode: string;
}

export type FetchDecision =
  | { kind: 'ignore' } // let the browser handle it (no respondWith)
  | { kind: 'cache-first'; key: string };

/**
 * Decide how to answer a fetch. Only same-origin GETs for precached shell
 * paths are answered from the cache; a navigation to any in-scope page path is
 * answered with the cached shell document so an installed app opens offline.
 * Everything else — cross-origin (daemon), non-GET, unknown paths, query
 * strings — is ignored and goes straight to the network.
 */
export function decide(request: RequestFacts, selfOrigin: string, precache: ReadonlySet<string>): FetchDecision {
  let url: URL;
  try {
    url = new URL(request.url);
  } catch {
    return { kind: 'ignore' };
  }
  if (url.origin !== selfOrigin) return { kind: 'ignore' };
  if (request.method !== 'GET') return { kind: 'ignore' };
  if (url.search !== '') return { kind: 'ignore' };
  if (request.mode === 'navigate') {
    // One-page app: every in-scope navigation renders the same shell.
    // `/__owner/*` and API-like paths are never served from the cache.
    if (/^\/(?:__owner|api|v\d+)(?:\/|$)/.test(url.pathname)) return { kind: 'ignore' };
    return precache.has('/') ? { kind: 'cache-first', key: '/' } : { kind: 'ignore' };
  }
  return precache.has(url.pathname) ? { kind: 'cache-first', key: url.pathname } : { kind: 'ignore' };
}

/** Messages the page may send to the waiting worker. */
export type PageToWorkerMessage = { type: 'SKIP_WAITING' };

export function isSkipWaiting(data: unknown): data is PageToWorkerMessage {
  return typeof data === 'object' && data !== null && (data as { type?: unknown }).type === 'SKIP_WAITING';
}
