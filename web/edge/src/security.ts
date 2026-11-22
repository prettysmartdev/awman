// The agent terminal's parser core (Ghostty, self-hosted WASM) is compiled
// with WebAssembly.compile, which browsers gate behind 'wasm-unsafe-eval'.
// Demonstrated under this exact policy: without it every terminal shows the
// load-failure notice. This allows WASM compilation only; JavaScript eval
// stays forbidden. Never add 'unsafe-eval'.
export const TERMINAL_REQUIRES_WASM_UNSAFE_EVAL = true;
/**
 * The only daemon destinations the hosted v0 webapp may reach (WI 0159 §4:
 * the same-machine boundary). Fixed: the page's hostname or protocol never
 * widens or moves them. Must match web/src/lib/client/http.ts.
 */
export const DAEMON_CONNECT_SOURCES = ['http://127.0.0.1:9877', 'ws://127.0.0.1:9877'] as const;
export const CONNECT_SRC = `connect-src 'self' ${DAEMON_CONNECT_SOURCES.join(' ')}`;
/** The document policy. Identical for every page URL (hosted, preview or local). */
export function contentSecurityPolicy(pageUrl?: string): string {
  void pageUrl; // deliberately unused: the page never selects connect-src
  return [
    "default-src 'none'",
    `script-src 'self'${TERMINAL_REQUIRES_WASM_UNSAFE_EVAL ? " 'wasm-unsafe-eval'" : ''}`,
    "style-src 'self'", "font-src 'self'", "img-src 'self'", "manifest-src 'self'",
    "worker-src 'self'", CONNECT_SRC,
    "base-uri 'none'", "form-action 'none'", "frame-ancestors 'none'", "object-src 'none'",
  ].join('; ');
}
export function secureResponse(response: Response, pageUrl?: string): Response {
  const headers = new Headers(response.headers);
  headers.set('Referrer-Policy', 'no-referrer');
  headers.set('Permissions-Policy', 'camera=(), microphone=(), geolocation=(), payment=(), usb=(), browsing-topics=()');
  headers.set('X-Content-Type-Options', 'nosniff');
  headers.set('Strict-Transport-Security', 'max-age=31536000; includeSubDomains');
  const type = headers.get('Content-Type')?.split(';')[0]?.trim().toLowerCase();
  // A scriptable non-HTML document navigated to directly runs sandboxed with
  // no capabilities (as images/icons, this header has no effect).
  if (type === 'image/svg+xml' || type === 'application/xml' || type === 'text/xml' || type === 'application/xhtml+xml') {
    headers.set('Content-Security-Policy', "default-src 'none'; sandbox");
  }
  if (type === 'text/html') {
    if (!pageUrl) throw new Error('HTML response requires the loaded page URL');
    headers.set('Content-Security-Policy', contentSecurityPolicy(pageUrl));
    headers.set('X-Frame-Options', 'DENY');
    // Each network navigation reaches the Worker. The service worker owns
    // offline shell caching explicitly; no browser HTTP-cache usage counts.
    headers.set('Cache-Control', 'no-store');
  }
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}
