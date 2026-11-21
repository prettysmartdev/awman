// Cooperative duplicate-tab handling for primary resume (WI 0160 C5).
//
// Exclusive Web Lock `awman-primary:` + JSON `[daemonEpoch,listenerEpoch,instanceId]`,
// requested with `ifAvailable` and never `steal`. A tab holding a matching
// resume hint retries for up to five seconds at the 50 ms–1 s backoff, because
// a reload can ask before the browser has released its previous document's
// lock. This is a UX coordination device, not authorization: the daemon
// remains the arbiter of primary admission.
import { backoffDelay } from './socket';

export const PRIMARY_LOCK_PREFIX = 'awman-primary:';
export const PRIMARY_LOCK_RETRY_MS = 5_000;

export interface PrimaryLockHandle {
  release(): void;
}

interface LockManagerLike {
  request(name: string, options: { mode: 'exclusive'; ifAvailable: true }, callback: (lock: unknown) => Promise<void> | void): Promise<unknown>;
}

export interface PrimaryLocker {
  /** False when the Web Locks API is unavailable. */
  readonly supported: boolean;
  /** One non-blocking attempt; null when another document holds the lock. */
  tryAcquire(name: string): Promise<PrimaryLockHandle | null>;
}

export function primaryLockName(daemonEpoch: string, listenerEpoch: string, instanceId: string): string {
  return `${PRIMARY_LOCK_PREFIX}${JSON.stringify([daemonEpoch, listenerEpoch, instanceId])}`;
}

function locksApi(): LockManagerLike | null {
  const locks = (globalThis.navigator as (Navigator & { locks?: LockManagerLike }) | undefined)?.locks;
  return locks && typeof locks.request === 'function' ? locks : null;
}

/** Web Locks backed locker (held until released or the document is destroyed). */
export function webLocker(api: LockManagerLike | null = locksApi()): PrimaryLocker {
  return {
    supported: api !== null,
    tryAcquire(name) {
      if (!api) return Promise.resolve(null);
      return new Promise(resolve => {
        let settled = false;
        api
          .request(name, { mode: 'exclusive', ifAvailable: true }, lock => {
            if (!lock) {
              settled = true;
              resolve(null);
              return;
            }
            return new Promise<void>(release => {
              settled = true;
              let released = false;
              resolve({
                release: () => {
                  if (released) return;
                  released = true;
                  release();
                },
              });
            });
          })
          .catch(() => {
            if (!settled) resolve(null);
          });
      });
    },
  };
}

export interface AcquireOptions {
  /** Retry for this long (ms) at the reconnect backoff; 0 = one attempt. */
  retryForMs: number;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
  /** Stop retrying (e.g. the connection was closed). */
  cancelled?: () => boolean;
}

/** Acquire with bounded retry; null when still held elsewhere (never queued, never stolen). */
export async function acquirePrimaryLock(locker: PrimaryLocker, name: string, options: AcquireOptions): Promise<PrimaryLockHandle | null> {
  const now = options.now ?? Date.now;
  const sleep = options.sleep ?? (ms => new Promise<void>(resolve => setTimeout(resolve, ms)));
  const started = now();
  for (let attempt = 1; ; attempt++) {
    const handle = await locker.tryAcquire(name);
    if (handle) return handle;
    if (options.cancelled?.()) return null;
    const delay = backoffDelay(attempt);
    if (now() - started + delay > options.retryForMs) return null;
    await sleep(delay);
    if (options.cancelled?.()) return null;
  }
}
