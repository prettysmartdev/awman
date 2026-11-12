'use client';
import { useEffect, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';

/**
 * Seconds until the next automatic authorization proof while the daemon has
 * rate limited it (429), else null. A throttle is temporary, never an ending:
 * show it as "retrying", not as revoked or ended.
 */
export function useRecoveryThrottle(client: DaemonClient): number | null {
  const remaining = () => {
    const until = client.recoveryThrottledUntil;
    return until === null || until === undefined ? null : Math.max(0, Math.ceil((until - Date.now()) / 1000));
  };
  const [seconds, setSeconds] = useState<number | null>(remaining);
  useEffect(() => {
    let timer: ReturnType<typeof setInterval> | null = null;
    const tick = () => {
      const until = client.recoveryThrottledUntil;
      const next = until === null || until === undefined ? null : Math.max(0, Math.ceil((until - Date.now()) / 1000));
      setSeconds(next);
      if (next === null && timer !== null) { clearInterval(timer); timer = null; }
    };
    const start = () => { tick(); if (timer === null) timer = setInterval(tick, 1000); };
    if (client.recoveryThrottledUntil != null) start();
    const off = typeof client.onRecoveryThrottled === 'function' ? client.onRecoveryThrottled(start) : () => undefined;
    return () => { off(); if (timer !== null) clearInterval(timer); };
  }, [client]);
  return seconds;
}

export function throttleText(seconds: number): string {
  return seconds > 0 ? `Too many attempts, retrying in ${seconds}s.` : 'Too many attempts, retrying now.';
}
