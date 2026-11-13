// Browser linking (design §2.4; WI 0160 C2). Started from a user gesture; the
// polling secret and the resulting grant stay in memory only. A key-bound
// link enrolls the public key of a fresh, already-persisted candidate record.
import { DaemonError, LinkDenied, LinkExpired, LinkGone, ProtocolError } from './errors';
import { bindingFromWire, grantFromWire, sameBinding, setGrant, type Grant, type ReauthBinding } from './grant';
import { HttpTransport } from './http';
import type { BrowserLinkCreateRequest, BrowserLinkCreated, BrowserLinkPollResponse } from './types';

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
  /** Key-bound links only: the binding the daemon confirmed at creation. */
  reauth?: ReauthBinding;
}

/** Key enrollment for one explicit link attempt (WI 0160 C2). */
export interface LinkEnrollment {
  credentialId: string;
  /** Raw uncompressed SEC1 public key, unpadded base64url. */
  publicKey: string;
  /** The probed listener binding the candidate record was committed under. */
  listener: string;
  daemonEpoch: string;
  listenerEpoch: string;
  /** Trusted page origin. */
  origin: string;
}

/** The daemon answered for another listener/generation than the probed one: abort and re-probe. */
export class EnrollmentMismatch extends Error {
  override readonly name = 'EnrollmentMismatch';
  constructor() {
    super('The daemon changed while this browser was being linked. Check the daemon, then try again.');
  }
}

/** The copyable owner-side approval command. */
export function linkCommand(request: Pick<LinkRequest, 'code'>): string {
  return `awman link ${request.code}`;
}

/**
 * POST /v1/browser-links; call only from a user gesture. Legacy body `{}`
 * creates a keyless (memory-only) link; with `enrollment` the body is the
 * exact C2 `reauth` object and the response must confirm the same binding.
 */
export async function createLink(transport: HttpTransport = new HttpTransport(), signal?: AbortSignal, enrollment?: LinkEnrollment): Promise<LinkRequest> {
  const request: BrowserLinkCreateRequest = enrollment
    ? { reauth: { version: 1, credential_id: enrollment.credentialId, algorithm: 'ECDSA_P256_SHA256', public_key: enrollment.publicKey } }
    : {};
  const { body } = await transport.json<BrowserLinkCreated>('/v1/browser-links', { method: 'POST', body: request, signal });
  if (!body || !LINK_CODE_RE.test(body.code) || typeof body.polling_secret !== 'string' || body.polling_secret.length < 32) {
    throw new ProtocolError('malformed browser-link response');
  }
  let reauth: ReauthBinding | undefined;
  if (enrollment) {
    const confirmed = bindingFromWire(body.reauth);
    const expected = { credentialId: enrollment.credentialId, daemonEpoch: enrollment.daemonEpoch, listenerEpoch: enrollment.listenerEpoch };
    if (!confirmed || !sameBinding(confirmed, expected) || body.listener !== enrollment.listener || body.origin !== enrollment.origin) throw new EnrollmentMismatch();
    reauth = confirmed;
  } else if (body.reauth !== undefined) {
    throw new ProtocolError('unexpected key binding on a keyless link');
  }
  return {
    ...(reauth ? { reauth } : {}),
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
      // A key-bound link must release a grant carrying exactly its binding; a
      // keyless link must not acquire one after approval.
      if (request.reauth ? !sameBinding(grant.reauth, request.reauth) : grant.reauth !== undefined) throw new ProtocolError('approved grant does not match the link binding');
      if (grant.origin !== request.origin || grant.listener !== request.listener) throw new ProtocolError('approved grant does not match the link');
      setGrant(grant);
      return grant;
    }
    if (response.status === 'denied') throw new LinkDenied();
    if (response.status !== 'pending') throw new ProtocolError('unknown browser-link poll status');
    deps.onPending?.();
    wait = Math.max(request.intervalMs, Number(response.retry_after_ms) || 0);
  }
}
