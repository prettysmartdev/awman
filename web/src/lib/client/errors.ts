// Typed errors surfaced by the browser client (design §5.4: every HTTP error
// body `{error, code}` is surfaced typed).
import type { ApiErrorCode } from './types';

/** A daemon HTTP error response (`{error, code}` body) or a protocol violation. */
export class DaemonError extends Error {
  override readonly name = 'DaemonError';
  constructor(
    readonly status: number,
    readonly code: ApiErrorCode,
    message: string,
    /** Seconds from `Retry-After`, when present. */
    readonly retryAfterSec: number | null = null,
  ) {
    super(message);
  }
}

/** The daemon could not be reached at all (network error, CORS refusal, LNA block, timeout). */
export class DaemonUnreachable extends Error {
  override readonly name = 'DaemonUnreachable';
  constructor(
    readonly detail: 'network' | 'blocked_by_browser' | 'timeout',
    message: string,
  ) {
    super(message);
  }
}

/** The response was not what the protocol allows (redirect, malformed JSON, oversized body). */
export class ProtocolError extends Error {
  override readonly name = 'ProtocolError';
}

export class LinkDenied extends Error {
  override readonly name = 'LinkDenied';
  constructor() {
    super('The link request was denied on this machine.');
  }
}

export class LinkExpired extends Error {
  override readonly name = 'LinkExpired';
  constructor() {
    super('The link code expired before it was approved.');
  }
}

/** 410 link_gone: consumed, expired or unknown (indistinguishable by design §2.4 D5). */
export class LinkGone extends Error {
  override readonly name = 'LinkGone';
  constructor() {
    super('The link request is no longer valid.');
  }
}

/** The browser grant was revoked or expired (401 browser_grant_invalid / close 1008). */
export class GrantRevoked extends Error {
  override readonly name = 'GrantRevoked';
  constructor(message = 'Browser access was revoked or expired.') {
    super(message);
  }
}

export function isAbortError(error: unknown): boolean {
  return error instanceof DOMException ? error.name === 'AbortError' : (error as { name?: unknown })?.name === 'AbortError';
}
