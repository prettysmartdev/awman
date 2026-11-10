// The one HTTP transport to the same-machine daemon (design §5.4).
//
// Every request: cache 'no-store', credentials 'omit', redirect 'error',
// mode 'cors', and `targetAddressSpace: 'loopback'` (Chrome Local Network
// Access; unknown RequestInit members are ignored by other browsers, so the
// hint is always sent). Bodies are JSON with `Content-Type: application/json`
// and bounded to 64 KiB (design §2.8). The grant travels only in the
// Authorization header — never in a URL, cookie or storage.
import { DaemonError, DaemonUnreachable, ProtocolError, isAbortError } from './errors';
import type { ApiErrorBody } from './types';

export const DAEMON_HTTP = 'http://127.0.0.1:9877';
export const DAEMON_WS = 'ws://127.0.0.1:9877';
export const MAX_REQUEST_BODY_BYTES = 64 * 1024;
/** Responses larger than this are refused (snapshots travel on the socket, not HTTP). */
export const MAX_RESPONSE_BYTES = 8 * 1024 * 1024;

export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PATCH' | 'DELETE';
  body?: unknown;
  /** Browser grant token (`awb_…`). */
  bearer?: string;
  signal?: AbortSignal;
  /** Abort after this many ms (default 10 s). */
  timeoutMs?: number;
  headers?: Record<string, string>;
}

export interface JsonResponse<T> {
  status: number;
  body: T;
  headers: Headers;
}

/** True when this browser understands the Local Network Access fetch hint. */
export function supportsTargetAddressSpace(): boolean {
  return typeof Request !== 'undefined' && 'targetAddressSpace' in Request.prototype;
}

function errorDetail(error: unknown): 'network' | 'blocked_by_browser' {
  const text = `${(error as { name?: string })?.name ?? ''} ${(error as { message?: string })?.message ?? ''}`.toLowerCase();
  return /security|mixed|insecure|private network|local network|address space|permission/.test(text) ? 'blocked_by_browser' : 'network';
}

function retryAfter(headers: Headers): number | null {
  const value = headers.get('Retry-After');
  if (!value) return null;
  const seconds = Number(value);
  return Number.isFinite(seconds) && seconds >= 0 ? seconds : null;
}

/**
 * Reads a body up to MAX_RESPONSE_BYTES. Content-Length is only a hint (it may
 * be absent, e.g. chunked encoding): the stream is read incrementally and
 * cancelled as soon as it exceeds the bound, so an oversized body is never
 * buffered in full.
 */
export async function readBounded(response: Response): Promise<string> {
  const length = Number(response.headers.get('Content-Length') ?? '0');
  if (length > MAX_RESPONSE_BYTES) {
    void response.body?.cancel().catch(() => undefined);
    throw new ProtocolError('daemon response too large');
  }
  if (!response.body) return '';
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let bytes = 0;
  let text = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    bytes += value.byteLength;
    if (bytes > MAX_RESPONSE_BYTES) {
      await reader.cancel().catch(() => undefined);
      throw new ProtocolError('daemon response too large');
    }
    text += decoder.decode(value, { stream: true });
  }
  return text + decoder.decode();
}

export class HttpTransport {
  constructor(
    private readonly fetchImpl: FetchLike = (input, init) => globalThis.fetch(input, init),
    readonly base: string = DAEMON_HTTP,
  ) {}

  /** Raw request; resolves for any HTTP status, rejects with DaemonUnreachable on network failure. */
  async raw(path: string, options: RequestOptions = {}): Promise<Response> {
    if (!path.startsWith('/v1/')) throw new ProtocolError(`refusing non-API path ${path}`);
    const headers: Record<string, string> = { Accept: 'application/json', ...options.headers };
    let body: string | undefined;
    if (options.body !== undefined) {
      body = JSON.stringify(options.body);
      if (new TextEncoder().encode(body).length > MAX_REQUEST_BODY_BYTES) throw new ProtocolError('request body exceeds 64 KiB');
      headers['Content-Type'] = 'application/json';
    }
    if (options.bearer) headers.Authorization = `Bearer ${options.bearer}`;
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(new DOMException('timeout', 'TimeoutError')), options.timeoutMs ?? 10_000);
    const onAbort = () => controller.abort(options.signal?.reason);
    options.signal?.addEventListener('abort', onAbort, { once: true });
    const init: RequestInit & { targetAddressSpace: 'loopback' } = {
      method: options.method ?? 'GET',
      headers,
      body,
      cache: 'no-store',
      credentials: 'omit',
      redirect: 'error',
      mode: 'cors',
      referrerPolicy: 'no-referrer',
      signal: controller.signal,
      targetAddressSpace: 'loopback',
    };
    try {
      const response = await this.fetchImpl(`${this.base}${path}`, init);
      if (response.type === 'opaqueredirect' || (response.status >= 300 && response.status < 400 && response.status !== 304)) {
        throw new ProtocolError('daemon answered with a redirect');
      }
      return response;
    } catch (error) {
      if (error instanceof ProtocolError) throw error;
      if (options.signal?.aborted) throw error;
      if (controller.signal.aborted || isAbortError(error)) throw new DaemonUnreachable('timeout', 'The daemon did not answer in time.');
      throw new DaemonUnreachable(errorDetail(error), 'The daemon could not be reached.');
    } finally {
      clearTimeout(timeout);
      options.signal?.removeEventListener('abort', onAbort);
    }
  }

  /** JSON request; non-2xx → DaemonError with the daemon's `{error, code}`. 204/304 → body null. */
  async json<T>(path: string, options: RequestOptions = {}): Promise<JsonResponse<T>> {
    const response = await this.raw(path, options);
    const text = response.status === 204 || response.status === 304 ? '' : await readBounded(response);
    let parsed: unknown = null;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        throw new ProtocolError(`daemon sent malformed JSON (HTTP ${response.status})`);
      }
    }
    if (!response.ok && response.status !== 304) {
      const body = (parsed ?? {}) as Partial<ApiErrorBody>;
      throw new DaemonError(
        response.status,
        typeof body.code === 'string' ? body.code : `http_${response.status}`,
        typeof body.error === 'string' ? body.error : `HTTP ${response.status}`,
        retryAfter(response.headers),
      );
    }
    return { status: response.status, body: parsed as T, headers: response.headers };
  }
}
