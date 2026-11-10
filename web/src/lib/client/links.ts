// Browser linking (design §2.4). Started from a user gesture; the polling
// secret and the resulting grant stay in memory only.
import { DaemonError, LinkDenied, LinkExpired, LinkGone, ProtocolError } from './errors';
import { grantFromWire, setGrant, type Grant } from './grant';
import { HttpTransport } from './http';
import type { BrowserLinkCreated, BrowserLinkPollResponse } from './types';

export const LINK_CODE_RE = /^web-[A-Z0-9]{4}-[A-Z0-9]{4}$/;

export interface LinkRequest {
  id: string;
  code: string;
  /** High-entropy secret; required for every poll; never displayed or stored. */
  pollingSecret: string;
  intervalMs: number;
  expiresAt: string;
  listener: string;
  origin: string;
}

/** The copyable owner-side approval command. */
export function linkCommand(request: Pick<LinkRequest, 'code'>): string {
  return `awman link ${request.code}`;
}

/** POST /v1/browser-links (body `{}`); call only from a user gesture. */
export async function createLink(transport: HttpTransport = new HttpTransport(), signal?: AbortSignal): Promise<LinkRequest> {
  const { body } = await transport.json<BrowserLinkCreated>('/v1/browser-links', { method: 'POST', body: {}, signal });
  if (!body || !LINK_CODE_RE.test(body.code) || typeof body.polling_secret !== 'string' || body.polling_secret.length < 32) {
    throw new ProtocolError('malformed browser-link response');
  }
  return {
    id: body.id,
    code: body.code,
    pollingSecret: body.polling_secret,
    intervalMs: Math.max(500, Number(body.interval_ms) || 2000),
    expiresAt: body.expires_at,
    listener: body.listener,
    origin: body.origin,
  };
}

export interface PollDeps {
  transport?: HttpTransport;
  now?: () => number;
  sleep?: (ms: number, signal: AbortSignal) => Promise<void>;
  /** Called after each pending poll (for "waiting for approval" UI). */
  onPending?: () => void;
}

function abortableSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(signal.reason);
      return;
    }
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal.reason);
    };
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

/**
 * Poll until approved (→ Grant, also installed as the in-memory grant),
 * denied (LinkDenied), expired (LinkExpired) or gone (LinkGone). Waits one
 * interval before every poll and honours 429 Retry-After.
 */
export async function pollLink(request: LinkRequest, signal: AbortSignal, deps: PollDeps = {}): Promise<Grant> {
  const transport = deps.transport ?? new HttpTransport();
  const now = deps.now ?? Date.now;
  const sleep = deps.sleep ?? abortableSleep;
  const expiresAt = Date.parse(request.expiresAt);
  let wait = request.intervalMs;
  for (;;) {
    await sleep(wait, signal);
    if (Number.isFinite(expiresAt) && now() >= expiresAt) throw new LinkExpired();
    let response: BrowserLinkPollResponse;
    try {
      ({ body: response } = await transport.json<BrowserLinkPollResponse>(`/v1/browser-links/${encodeURIComponent(request.id)}/poll`, {
        method: 'POST',
        body: { polling_secret: request.pollingSecret },
        signal,
      }));
    } catch (error) {
      if (error instanceof DaemonError && error.status === 429) {
        wait = Math.max(request.intervalMs, (error.retryAfterSec ?? 2) * 1000);
        continue;
      }
      if (error instanceof DaemonError && error.status === 410) {
        throw Number.isFinite(expiresAt) && now() >= expiresAt ? new LinkExpired() : new LinkGone();
      }
      throw error;
    }
    if (response.status === 'approved') {
      const grant = grantFromWire(response.grant);
      setGrant(grant);
      return grant;
    }
    if (response.status === 'denied') throw new LinkDenied();
    if (response.status !== 'pending') throw new ProtocolError('unknown browser-link poll status');
    deps.onPending?.();
    wait = Math.max(request.intervalMs, Number(response.retry_after_ms) || 0);
  }
}
