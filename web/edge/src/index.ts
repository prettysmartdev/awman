import { navigationDigests, ownerAuthorized } from './privacy';
import { periodsAt } from './periods';
import { secureResponse } from './security';
import type { UsageCounter } from './usage';
export { UsageCounter } from './usage';
export interface Env {
  ASSETS: Fetcher;
  USAGE: DurableObjectNamespace<UsageCounter>;
  USAGE_HMAC_SECRET: string;
  OWNER_EXPORT_SECRET: string;
}
export const EXPORT_PATH = '/__owner/usage';
export const COUNTER_NAME = 'usage-v1';

export function isApiPath(path: string): boolean {
  return /^\/+(?:api|v[0-9]+|health|healthz|metrics|__owner)(?:\/|$)/i.test(path);
}
export function isNavigation(request: Request, path: string): boolean {
  if (request.method !== 'GET' || isApiPath(path)) return false;
  // Fetch Metadata deliberately excludes fetch(), frames, SW precache,
  // subresources, and speculative loads. No UA guessing/fallback identifiers.
  if (request.headers.get('Sec-Fetch-Mode') !== 'navigate'
    || request.headers.get('Sec-Fetch-Dest') !== 'document') return false;
  if (!request.headers.get('Accept')?.toLowerCase().includes('text/html')) return false;
  if (/prefetch|prerender/i.test(`${request.headers.get('Purpose') ?? ''} ${request.headers.get('Sec-Purpose') ?? ''}`)) return false;
  if (request.headers.has('RSC') || request.headers.has('Next-Router-Prefetch')) return false;
  const last = path.split('/').at(-1) ?? '';
  return (!last.includes('.') || last === 'index.html')
    && !/^\/+(?:_next|assets|icons|\.well-known)(?:\/|$)/i.test(path)
    && !/^\/+(?:sw|service-worker|manifest)(?:[./-]|$)/i.test(path)
    && !/^\/+(?:404|500|_error|_not-found|offline)(?:[./]|$)/i.test(path);
}
/** Longest a page response waits for its count before being served anyway. */
export const COUNT_WAIT_MS = 1_000;

// One count per navigation, at most once: a stale-period attempt writes
// nothing and is resampled once; any other result or failure is dropped
// (an undercount), never retried and never surfaced to the visitor.
async function countNavigation(request: Request, env: Env): Promise<void> {
  const counter = env.USAGE.get(env.USAGE.idFromName(COUNTER_NAME));
  for (let attempt = 0; attempt < 2; attempt++) {
    const periods = periodsAt(Date.now());
    const digests = await navigationDigests(request.headers.get('CF-Connecting-IP'), env.USAGE_HMAC_SECRET, periods);
    if (await counter.record({ periods, digests }) !== 'period_changed') return;
  }
}

function fixedResponse(status: number, body: string): Response {
  return secureResponse(new Response(body, { status, headers: { 'Cache-Control': 'no-store', 'Content-Type': 'text/plain; charset=utf-8' } }));
}

export default {
  async fetch(request: Request, env: Env, ctx?: ExecutionContext): Promise<Response> {
    try {
      const url = new URL(request.url);
      let path: string;
      try { path = decodeURIComponent(url.pathname); } catch { return fixedResponse(400, 'Bad request'); }
      if (path.includes('\\') || path.includes('\0')) return fixedResponse(400, 'Bad request');
      if (path === EXPORT_PATH) {
        // HTTPS only: a bearer sent over plain HTTP is refused, never honoured.
        if (url.protocol !== 'https:' || request.method !== 'GET' || url.search || request.headers.has('Origin')
          || !await ownerAuthorized(request.headers.get('Authorization'), env.OWNER_EXPORT_SECRET)) {
          return fixedResponse(403, 'Forbidden');
        }
        const usage = await env.USAGE.get(env.USAGE.idFromName(COUNTER_NAME)).exportAggregates();
        if (!usage) return fixedResponse(503, 'Service unavailable');
        return secureResponse(Response.json(usage, { headers: { 'Cache-Control': 'no-store' } }));
      }
      if (isApiPath(path)) return fixedResponse(404, 'Not found');
      if (request.method !== 'GET' && request.method !== 'HEAD') return fixedResponse(405, 'Method not allowed');
      // Static binding only: no cloud-to-daemon proxy, server rendering or
      // forwarding of visitor headers, query strings, credentials or bodies.
      const assetHeaders = new Headers();
      for (const name of ['Accept', 'Accept-Encoding', 'Range', 'If-None-Match', 'If-Modified-Since']) {
        const value = request.headers.get(name);
        if (value) assetHeaders.set(name, value);
      }
      const response = await env.ASSETS.fetch(new Request(`${url.origin}${url.pathname}`, { method: request.method, headers: assetHeaders, redirect: 'manual' }));
      const html = response.headers.get('Content-Type')?.split(';')[0]?.trim().toLowerCase() === 'text/html';
      if (response.status === 200 && html && isNavigation(request, path)) {
        // Counting fails open: the public site never depends on the usage
        // counter. A slow counter delays the page at most COUNT_WAIT_MS; the
        // count may still finish under waitUntil. Errors only undercount.
        const counting = countNavigation(request, env).catch(() => undefined);
        ctx?.waitUntil(counting);
        let timer: ReturnType<typeof setTimeout> | undefined;
        await Promise.race([counting, new Promise<void>(resolve => { timer = setTimeout(resolve, COUNT_WAIT_MS); })]);
        clearTimeout(timer);
      }
      return secureResponse(response);
    } catch {
      return fixedResponse(503, 'Service unavailable');
    }
  },
} satisfies ExportedHandler<Env>;
