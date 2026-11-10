/// <reference lib="webworker" />
// Service worker source (design §5.8). Bundled to out/sw.js by
// scripts/build-sw.mjs, which injects the precache manifest.
//
// - install: fetch the whole shell into a new versioned cache, all-or-nothing
//   (Cache.addAll rejects if any file fails, leaving the previous version in
//   charge). Entries are ordinary same-origin fetches (Sec-Fetch-Dest: empty),
//   which the edge Worker never counts as page loads.
// - no automatic skipWaiting once a version is in control: the page shows
//   "Update available — reload" and posts SKIP_WAITING; every open page then
//   reloads on `controllerchange`, so an old shell never runs with new chunks.
// - activate: delete older shell caches, claim clients.
// - fetch: cache-first for precached same-origin shell paths only. No
//   background revalidation of documents, no background sync, no push, no
//   analytics, and never any cross-origin (daemon) request.
import { cacheNameFor, decide, isSkipWaiting, staleCaches, type PrecacheManifest } from './policy';

declare const self: ServiceWorkerGlobalScope;
declare const __AWMAN_PRECACHE__: PrecacheManifest;

const manifest = __AWMAN_PRECACHE__;
const cacheName = cacheNameFor(manifest.version);
const precache = new Set(manifest.urls);

self.addEventListener('install', event => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(cacheName);
      await cache.addAll(manifest.urls.map(url => new Request(url, { cache: 'reload', credentials: 'omit', redirect: 'error' })));
      // First install (nothing in control yet): activate right away.
      if (!self.registration.active) await self.skipWaiting();
    })(),
  );
});

self.addEventListener('activate', event => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(staleCaches(names, manifest.version).map(name => caches.delete(name)));
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('message', event => {
  if (isSkipWaiting(event.data)) void self.skipWaiting();
});

self.addEventListener('fetch', event => {
  const request = event.request;
  const decision = decide({ url: request.url, method: request.method, mode: request.mode }, self.location.origin, precache);
  if (decision.kind === 'ignore') return;
  event.respondWith(
    (async () => {
      const cache = await caches.open(cacheName);
      const hit = await cache.match(decision.key);
      if (hit) return hit;
      // Not cached (e.g. a partially evicted cache): plain network fetch,
      // never written back, so the cache only ever holds one whole version.
      return fetch(request);
    })(),
  );
});
