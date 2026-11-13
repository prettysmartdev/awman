// Service-worker registration and atomic update flow (design §5.8).
// - registered only in production builds, after the page has loaded once,
//   and only on https: or localhost;
// - a new version waits until the user chooses "Reload to update"; then every
//   open page reloads on controllerchange, so an old shell never runs with
//   new chunks.

export interface ShellUpdate {
  /** Activate the waiting version; all pages reload. */
  apply(): void;
}

export function serviceWorkerAllowed(loc: Pick<Location, 'protocol' | 'hostname'> = location): boolean {
  return loc.protocol === 'https:' || loc.hostname === 'localhost' || loc.hostname === '127.0.0.1';
}

export function registerShellWorker(onUpdateReady: (update: ShellUpdate) => void, reload: () => void = () => location.reload()): () => void {
  if (process.env.NODE_ENV !== 'production') return () => undefined;
  if (typeof navigator === 'undefined' || !('serviceWorker' in navigator) || !serviceWorkerAllowed()) return () => undefined;
  let cancelled = false;
  let reloading = false;
  const container = navigator.serviceWorker;

  const offerUpdate = (worker: ServiceWorker) => {
    if (cancelled) return;
    onUpdateReady({ apply: () => worker.postMessage({ type: 'SKIP_WAITING' }) });
  };

  // Whether this page is controlled is tracked dynamically: the first
  // acquisition (first install claims an uncontrolled page) must not reload,
  // but every later replacement must, including in a page that was open
  // during the first install. Otherwise an approved update would delete the
  // old cache under a still-running old shell.
  let controlled = !!container.controller;
  const onControllerChange = () => {
    if (reloading || cancelled) return;
    if (!controlled) { controlled = !!container.controller; return; }
    reloading = true;
    reload();
  };
  container.addEventListener('controllerchange', onControllerChange);

  const register = async () => {
    try {
      const registration = await container.register('/sw.js', { scope: '/', updateViaCache: 'none' });
      if (cancelled) return;
      if (registration.waiting && container.controller) offerUpdate(registration.waiting);
      registration.addEventListener('updatefound', () => {
        const installing = registration.installing;
        if (!installing) return;
        installing.addEventListener('statechange', () => {
          if (installing.state === 'installed' && container.controller) offerUpdate(installing);
        });
      });
    } catch {
      // Offline installs are best-effort; the app works without a worker.
    }
  };

  const start = () => void register();
  if (document.readyState === 'complete') start();
  else window.addEventListener('load', start, { once: true });
  return () => {
    cancelled = true;
    window.removeEventListener('load', start);
    container.removeEventListener('controllerchange', onControllerChange);
  };
}
