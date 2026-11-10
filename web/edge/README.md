# awman app-shell Worker

Independent pnpm package; no Rust build, Next server runtime, API adapter or
cloud-to-daemon traffic. The Next frontend must statically export to `web/out`.
Workers Static Assets serves that directory through `ASSETS`; `run_worker_first`
ensures every HTML response receives CSP and every qualifying document request
passes the counter. Native HTML canonical redirects stay redirects; missing
paths return the exported 404 page, never an API-to-SPA fallback.

Use Node 22.13+ (or a compatible newer release) and the exact pnpm version in
`package.json`:

```sh
pnpm --dir web/edge install --frozen-lockfile
pnpm --dir web/edge run check
# After the frontend build creates web/out:
pnpm --dir web/edge run verify:assets
pnpm --dir web/edge exec wrangler secret put USAGE_HMAC_SECRET
pnpm --dir web/edge exec wrangler secret put OWNER_EXPORT_SECRET
pnpm --dir web/edge run deploy
```

Supply two independent random secrets with at least 32 bytes of entropy. Never
commit secrets. Local Wrangler development can use ignored `.dev.vars`. Keep
the HMAC secret stable within an open month: changing it mid-period overcounts
addresses already seen in that period. Do not add production secrets to test
bindings or `vars`. The deploy command verifies the export before Wrangler runs.
No deployment is performed by `check`.

## Counting and storage

Only successful (200) HTML `GET` responses to top-level document navigations
(`Sec-Fetch-Mode: navigate`, `Sec-Fetch-Dest: document`, HTML Accept) count.
Requests without Fetch Metadata deliberately do not count; this can undercount
older browsers. Frames, fetch/RSC requests, prefetch/prerender, HEAD, redirects,
errors, exported error/offline pages, assets, manifests, service workers and reserved API/health paths do not
count. Offline cache hits never reach the Worker. The service worker must
precache with ordinary `fetch()` requests, not synthesize document-navigation
metadata or perform navigation background revalidation/telemetry. There is no
public counting endpoint, beacon, cookie, visitor ID or client telemetry.

`CF-Connecting-IP` is the sole input. The parser accepts full strict dotted IPv4
and IPv6 (including dotted IPv4 suffixes), rejecting ports, zone IDs, lists and
ambiguous IPv4 notations. IPv6 canonicalizes to eight lowercase padded groups;
IPv4-mapped IPv6 remains IPv6 and all equivalent IPv6 spellings deduplicate.
Never truncate addresses. `X-Forwarded-For` is ignored. Before calling the DO,
the Worker computes HMAC-SHA-256 over exactly `day|YYYY-MM-DD|<canonical-IP>`
and `month|YYYY-MM|<canonical-IP>`. Only those two transient digest byte arrays
and UTC period labels cross the binding. No visitor header, URL, raw IP,
credential, request ID or individual digest enters storage. Invalid/missing
connecting addresses still increment loads and missing-IP totals, without
updating either sketch. Static asset calls also omit visitor headers and queries.

One named SQLite-backed Durable Object, `usage-v1`, serializes updates in one
strongly consistent storage transaction: maintenance, daily/monthly page loads,
missing-IP totals, both sketch changes and the next alarm all commit or roll
back together. Do not shard it by day: both sketches must update atomically.
HMAC digests update HLL registers directly; they are never persisted separately.
If hashing crosses midnight, a stale-period call performs no write; the Worker
resamples and rehashes once. Counting fails open: the page is always served, and
a count that fails (storage/RPC error, misconfigured secret) is dropped without
retrying a potentially committed write, so failures only undercount. A slow
counter delays a page by at most one second; the count may still complete in the
background (`waitUntil`).

HLL precision is **12**: **4,096 one-byte registers** per open UTC day/month,
with expected asymptotic standard error **1.625%**. Below 2.5m use linear counting
`m * ln(m / zero-registers)` when empty registers remain. Day and calendar-month
sketches are independent; monthly distinct IPs are not the sum of daily estimates.
The tests check three deterministic independent streams at cardinalities 1, 10,
100, 1,000, 10,000 and 100,000 with a 6% error bound, plus exact empty/singleton
and duplicate behavior. This is an empirical bound, not a per-result guarantee.

The first transaction/alarm after UTC midnight finalizes closed sketches and
removes their register arrays, retaining only period, totals and estimate.
Daily alarms continue during inactivity; export and count transactions also
apply maintenance. Platform alarm delays can delay physical deletion until the
next execution. Rows expire **13 calendar months after period end**, clamping
end-of-month dates. The empty object stops scheduling alarms. At most one day
and one month normally have sketches; retained rows have no visitor records.

## Owner export

`GET https://awman.prettysmart.dev/__owner/usage` requires the separate secret
in `Authorization: Bearer <OWNER_EXPORT_SECRET>`. Use an owner-side HTTP client;
browser Origin headers, query strings, other methods and unauthorized requests
are refused, as is any request over plain `http:`. Do not put the secret in a URL
or in frontend code. Export responses
are `Cache-Control: no-store`, have no CORS allowance, and contain only:

```json
{
  "timezone": "UTC",
  "distinctIpLabel": "approximate distinct IPs",
  "rows": [
    { "kind": "day", "period": "2026-11-09", "pageLoads": 8, "missingIpCount": 1, "distinctIpEstimate": 5.003 }
  ]
}
```

`kind` is `day` or `month`; month periods use `YYYY-MM`. Open-period estimates
can change. Sketches and HMACs are never exported. These are approximate distinct
**IPs**, not people: NAT can combine people and changing addresses can count one
person repeatedly. Export itself does not increment usage.

## Deployment/privacy requirements

- In the Cloudflare zone, set **Network → Pseudo IPv4 → Off**. In particular,
  never use Overwrite Headers, which replaces IPv6 in `CF-Connecting-IP`. This
  zone setting cannot be enforced by the Worker and must be verified on deploy.
  Do not route through another same-zone Worker that can rewrite connecting IPs;
  the custom domain must terminate directly on this Worker.
- In the zone, enable **SSL/TLS → Edge Certificates → Always Use HTTPS**. Every
  response also carries `Strict-Transport-Security: max-age=31536000;
  includeSubDomains`.
- Never run `wrangler tail` (or any live tail) against production: it would show
  request metadata and the owner's `Authorization` header. If a tail was attached
  during an export, rotate `OWNER_EXPORT_SECRET`.
- Anyone holding both the stored open-period sketch and `USAGE_HMAC_SECRET` could
  test, probabilistically, whether a given IP visited in the open day/month.
  Keep that secret out of routine tooling; closed-period sketches are deleted.
- Wrangler disables observability, invocation logs, traces, Logpush, source-map
  uploads and tool metrics; the source has no logging, analytics binding, tail
  consumer or external fetch. Keep dashboard settings consistent. Do not enable
  request logs, Logpush jobs, analytics/telemetry scripts, tracing or tailing on
  this deployment. `check` verifies the checked-in configuration and exact pins;
  its top-level keys are an allowlist, so tail consumers, analytics/storage/queue
  bindings, `vars` or per-environment `env.*` blocks fail the check.
- Cloudflare still processes network/request metadata (including source IPs) to
  serve and secure the site, outside this application's aggregate store. This
  implementation cannot erase or promise the absence of Cloudflare platform
  metadata, account-level logging or independently enabled zone products.
- CSP allows self-hosted resources and only the fixed loopback HTTP/WebSocket
  origins in addition to self for connections. It forbids inline scripts/styles,
  JavaScript eval, forms, objects and framing. Referrer-Policy is `no-referrer`;
  Permissions-Policy disables unrelated hardware/browser features. Keep the
  production frontend compatible with this policy; do not weaken it for default
  Next hydration/style output. Extract that generated code into local assets.
- `TERMINAL_REQUIRES_WASM_UNSAFE_EVAL` in `src/security.ts` is **true**: the
  self-hosted Ghostty terminal core is WebAssembly, and browsers refuse to
  compile it without `'wasm-unsafe-eval'` (every terminal would show the
  load-failure notice). It allows WASM compilation only, never `unsafe-eval`.
  `verify:assets` fails if the export ships `.wasm` without that allowance, or
  if `'unsafe-eval'` ever appears.
- `verify:assets` parses every exported HTML file, rejecting inline scripts,
  styles, handlers, `javascript:` URLs, remote resources (every `srcset`
  candidate and every `<link>`, including preconnect/dns-prefetch), embeds and
  missing script/style resources. SVG/XML files with scripts or handlers are
  rejected, and such documents are served with `default-src 'none'; sandbox`.
  It also rejects symlinks and public source maps and writes a sorted SHA-256
  build inventory to ignored `.artifacts/asset-integrity.json` for release review.
  It is a build gate, not a substitute for browser CSP/offline tests. The final
  frontend/browser checks belong to the integration step, once `web/out` exists.

## Validation

`check` runs strict TypeScript, ESLint (including a no-console rule), configuration
checks, and Vitest in Cloudflare's real workerd/SQLite Durable Object test pool.
Coverage includes 100 simultaneous navigations, atomic rollback when the second
row fails, IPv4/IPv6 parsing, deduplication, daily/monthly boundaries, alarms,
retention without traffic, owner authentication, canonical redirects, missing-IP
counts, HTML security headers, exported fields and scans of persisted state,
exports, static-binding requests and sanitized errors for raw IPs/digests/secrets.
The test asset directory is isolated here; tests never need or modify `web/out`.
Vitest 4 and the current Cloudflare Vitest plugin are pinned with Wrangler for
reproducibility. No production runtime dependency is bundled.

Cloudflare references used for this setup:
[Static Assets](https://developers.cloudflare.com/workers/static-assets/),
[Worker-first routing](https://developers.cloudflare.com/workers/static-assets/routing/worker-script/),
[Next.js options](https://developers.cloudflare.com/workers/framework-guides/web-apps/nextjs/),
[Durable Object storage](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/),
[Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/),
[incoming headers](https://developers.cloudflare.com/fundamentals/reference/http-headers/).
A static export needs the native asset binding; no full-stack Next adapter is
required for this package.
