# Work Item: Task

Title: Webapp UX overhaul and ethereal glass design
Issue: none

Depends on: 0159 (webapp v0). Complete this item before 0161's engine parity and deployed-origin gates.

## Summary

- Replace the webapp's crowded utility-first layout with a coherent browser-native workspace experience. Keep functional parity with the TUI, including commands, dialogs, tabs, terminals, squad, queue, workflow, Git, configuration, copy, connection and recovery, without reproducing the TUI's layout or key-driven interaction model.
- Establish an original visual language inspired by liquid glass: an ethereal ambient canvas, translucent chrome, soft depth and light, restrained color, precise typography, and clear active and danger states. Content-heavy surfaces such as logs, terminals and forms must remain opaque enough to read.
- Make the experience usable in a normal desktop window without stretching it across an ultrawide monitor. Remove overlapping controls, make terminal windows resize their actual agent PTYs, and support narrow touch screens as well as large desktops.
- Refresh an already linked browser without requiring another `awman link web-<code>` approval while the authorization remains valid and the daemon is available.

## User Stories

### User Story 1
As a browser user, I want to move between workspaces, active agents, execution, workflow, Git and other tools without memorizing TUI keys, so I can understand what is happening and act quickly.

### User Story 2
As a user connecting a new browser, I want a calm, legible setup flow with one obvious next action and copyable commands, so I can reach a workspace without learning the architecture first.

### User Story 3
As a user on a phone or with accessibility preferences, I want readable content, touch-sized targets, predictable focus, and reduced-motion behavior, so the same functionality remains usable.

### User Story 4
As a user running an agent TUI in the browser, I want its rows and columns to follow the space actually available after I resize the window or change panels, so the agent redraws correctly and stays usable.

### User Story 5
As a user returning to an open workspace, I want a page refresh to reconnect without asking me to repeat the owner-side link command, so a routine reload does not interrupt my work.

## Implementation Details

1. Audit `web/acceptance/action-matrix.json` against the current TUI and shared command catalogue before redesigning. Record each user goal and its web entry point. Every supported TUI action needs a discoverable web control or flow; shortcuts may accelerate but never be the only route. Keep daemon frames, command meanings, dialog order and first-answer-wins behavior unchanged.
2. Map the primary journeys: connect and link → choose or resume a workspace → select a tab → run a command or use an agent → inspect output and status → open workflow, Git, squad or queue when needed → recover from disconnect. Give each screen one clear primary action. Group secondary and destructive actions by purpose and context instead of scattering buttons across the tab bar, status row, agent cards, log and composer. Use a command menu, contextual menu, inspector or drawer where appropriate, with clear labels and selected states.
3. Redesign the information architecture for **ordinary desktop widths first**. At 1280×720 and 1440×900, the active terminal or execution area, tabs, status and composer must fit without page-level horizontal scrolling, clipped text or overlapping controls. A Git or workflow inspector must not compress the main area into an unusable strip; avoid simultaneously reserving permanent width for multiple side panels. At ultrawide widths, use the extra room deliberately rather than stretching every control. Adapt through 1024×768, 768×1024 and 390×844; at narrow widths show one main panel at a time with obvious navigation and persistent access to the active agent and composer.
4. Introduce shared light and dark design tokens for color, type, spacing, radii, borders, shadows and glass. Use an original Apple liquid-glass-inspired aesthetic: luminous ambient background, soft translucent navigation surfaces, restrained highlights, generous whitespace and clear depth. Keep text, forms, terminal cells, logs and critical status on highly legible surfaces. Use external CSS under the existing CSP; avoid remote fonts, inline styles and ornamental effects that interfere with performance or reading. Provide opaque fallbacks for missing backdrop filters and reduced-transparency preferences.
5. Redesign connect, recovery and workspace selection as a calm entry flow. Preserve permission, linking, recheck, setup, attach and recovery actions and their error messages. Give terminals, execution logs, workflow cards, Git changes, squad, queue, dialogs and notices consistent hierarchy, spacing, overflow behavior and visual states.
6. Fix terminal geometry end to end. Observe the *actual rendered terminal viewport* and cell metrics, then send debounced `awman.terminal.v1` resize frames when window size, layout, panel selection, zoom/font metrics, terminal visibility or control ownership changes. The agent PTY must receive new columns and rows and redraw, including after reconnect. Do not send zero-sized dimensions or resize requests from a viewer without the input grant. Keep parallel terminals independent.
7. Replace the memory-only browser-grant lifecycle that currently forces a new `awman link` after refresh. Design a reload-safe, origin-bound and listener-bound reauthorization mechanism that preserves expiry, revocation and one-primary-instance resume. Prefer a device-bound proof or another reviewable mechanism over persisting a raw bearer grant. Explicitly reconcile this change with 0159's rule against storing grants in browser storage; document the chosen credential lifecycle and threat model before implementation. A reload within the valid authorization window must reconnect without owner approval; expiry, revocation or daemon restart may require re-linking with an accurate explanation.

## Credential Lifecycle and Threat Model

This is the normative design for item 7, to be security-reviewed before
implementation. It changes browser authorization HTTP only; `awman.frontend.v1`,
`awman.terminal.v1`, command semantics, dialog order and engine exit policy remain
unchanged. The owner explicitly allows persisting short-lived or refresh tokens
in localStorage and prefers avoiding cookies. The chosen mechanism uses an
IndexedDB-held, non-extractable signing key instead. This is a deliberate, narrow
amendment to [0159's storage contract](0159-webapp-v0.md#storagesecurity-amendment--wi-0160),
not a claim that persistent credentials are categorically forbidden.

### C1. Authority, key and lifetime

One owner approval creates one engine-scope grant, with the existing absolute
twelve-hour lifetime measured from approval. A browser generates an ECDSA P-256
key pair with `crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"}, false,
["sign","verify"])`. Its private key must be non-extractable, have only `sign`
usage, and survive a write/read/sign/verify round trip through IndexedDB before
link creation. The public key is exported as raw uncompressed SEC1 bytes. The
daemon binds that public key immutably to the approved grant, exact page Origin,
receiving listener name and listener generation. No endpoint can replace a key
on an existing grant. New key, origin or listener requires a new owner approval.
Every explicit link attempt generates a fresh key pair and credential ID. A key
already in IndexedDB is used only to prove its own record; it is never enrolled
again, so a record planted or altered by same-origin script cannot become the
key of a later approval.

Successful proof recovers the **same grant ID, bearer and original expiry**.
It creates neither a new grant nor a sliding expiration. Independent tabs may
recover this grant without rotating each other's bearer. The raw `awb_` bearer
stays in each document's memory. For key-bound grants only, the daemon retains
the originally minted bearer in a secret-typed, non-logged memory slot until
expiry/revocation/stop, so it can return it again. Its on-disk grant record still
contains only the existing token hash and metadata. This extends daemon-memory
retention beyond the current one-use poll; it is an explicit tradeoff, not token
rotation. A stolen live bearer retains the existing twelve-hour maximum exposure.
The daemon records both the UTC expiry and a twelve-hour monotonic deadline at
approval; reaching either ends authorization, so a backward wall-clock adjustment
cannot extend it. Browser clock or persisted timestamps never decide validity.
Both deadlines are evaluated at every bearer check, ticket use, proof release and
primary admission. Live sockets must also end at the earlier deadline: a timer
armed once does not fire on time across a system suspend (monotonic clocks stop
while wall time advances), so each grant watch re-evaluates both clocks at least
every 30 seconds and when the process observes a clock jump. The existing
single-sleep watch does not meet this and must change.

The signing key is browser-profile/origin bound by the browser storage boundary;
this is not hardware attestation, user verification, or a guarantee against
profile copying, extensions, local malware or compromised same-origin script.
Non-extractability prevents ordinary script key export, not signing by malicious
script running with the origin's authority. The browser may evict its storage.
Non-extractability is an API property, not encryption at rest: browsers write
the key material to the profile directory in a recoverable form. Anything that
can read those files and also reach the loopback listener with a forged Origin
header can recover the grant until it ends. Against that reader the key is no
stronger than a stored bearer; the protection is the unchanged twelve-hour and
daemon-restart limit, owner revocation and the profile's file permissions.
Storage failure must not fall back to exporting a private key or silently saving
a bearer. Offer the existing memory-only link flow with the explicit C7 warning.
There are no cookies, background refresh, cloud credentials or new custom daemon
endpoints selected by browser storage.

The algorithm and structured-key serialization use the
[Web Cryptography Recommendation](https://www.w3.org/TR/2017/REC-WebCryptoAPI-20170126/#ecdsa-operations).
All cryptographic operations use platform primitives and an audited, pinned Rust
implementation; do not implement ECDSA arithmetic in application code.

### C2. HTTP ownership, negotiation and enrollment

Layer 0 `src/data/browser_auth.rs` owns the new DTOs/constants. Layer 1
`src/engine/auth/` owns key verification, nonce generation, bounded challenge
tables and secret storage. Layer 2 `src/command/daemon_api/browser.rs` owns
eligibility, grant/key binding, lifecycle, error decisions and resume authority.
Layer 3 `src/frontend/api/browser.rs` decodes/encodes these DTOs;
`src/frontend/api/routes.rs` and the existing operation/capability catalogue
register/gate them. No business authorization belongs in React or HTTP handlers.

The existing allowed-Origin browser `GET /v1/health` response gains:

```json
{
  "protocols": {"api":1,"frontend":1,"terminal":1,"browser":1,"browser_reauth":1},
  "browser_reauth": {"version":1,"daemon_epoch":"<epoch>","listener_epoch":"<epoch>"}
}
```

These are additions to the existing health fields, not its entire body.
Also advertise capability `browser_reauth`. Advertise the protocol, capability
and object only on an eligible loopback browser listener with engine service.
Each epoch is 16 independently random bytes, base64url without padding (22
characters). `daemon_epoch` is fresh on every process start, including crash
recovery. `listener_epoch` is fresh on bind/rebind/re-enable/recreation or transport
replacement, and on **every** event that revokes all of a listener's browser
grants without a rebind (listener disabled, `engine` removed from its features,
listener removed). Those settings apply live, so the epoch cannot be derived from
the socket bind alone; it is the generation of the listener's grant set. It is
not the listener's name. Both are public identifiers, not
credentials or authenticated server identity. An origin-only removal revokes
affected grants; an unrelated allowed-origin addition does not change epochs.
Epochs come from the operating system CSPRNG and are never persisted.
Existing listener/engine shutdown revocation rules still apply.

The new client requires `protocols.browser_reauth === 1` and matching capability
before enrollment/recovery with a stored key. An incompatible daemon produces C7's
version state; it cannot silently downgrade that credential. Legacy `{}` links
remain memory-only and cannot acquire a key after approval. Older clients ignore
the additional health fields. The frontend/terminal protocol versions stay 1.

All requests below use the existing fixed daemon base and transport:
`Content-Type: application/json`, `Accept: application/json`, `cache: no-store`,
`credentials: omit`, `redirect: error`, `mode: cors`, no referrer. All responses,
including failures and health, are `Cache-Control: no-store`. No ETags for auth.
No query parameters are accepted on the new routes. The existing 64 KiB body
limit, exact Origin/Host validation, loopback-only rule, CORS/preflight and
feature/method allowlists apply before policy processing. Never trust a body
Origin/listener as request authority; compare it with the gated context.
Disallowed/missing/null/duplicate Origin or invalid Host gets the existing 403
without CORS. Nonbrowser/owner/TLS-listener access to new routes is 404. They do
not accept API keys, worker credentials, cookies or bearer fallback.

`POST /v1/browser-links` accepts either the existing absent/empty object or exactly:

```json
{"reauth":{"version":1,"credential_id":"<32-byte-base64url>","algorithm":"ECDSA_P256_SHA256","public_key":"<65-byte-base64url>"}}
```

The client generates `credential_id` with 32 random bytes (43 characters), never
reuses it, and commits its key/ID and health binding to IndexedDB **before** sending
this request. The public key must decode to exactly `0x04 || X[32] || Y[32]`, a
valid finite P-256 point. Reject noncanonical base64url, padding, alternate curves,
compressed points, unknown fields (including nested ones), private material and
caller-selected scope/origin/listener with `400 invalid_request`. A credential ID
already reserved or bound anywhere in this daemon gets `409 credential_exists`;
it never replaces the existing key. The client generates a new record only for a
new explicit link attempt, not as an automatic response to this error.

For key-bound requests, the existing 201 link body gains:

```json
{"reauth":{"version":1,"credential_id":"<id>","daemon_epoch":"<epoch>","listener_epoch":"<epoch>"}}
```

Before accepting the create response, compare its origin/listener/epochs/ID with
the precommitted candidate and probed binding. A restart/rebind during enrollment
aborts that attempt and returns to probing; do not silently overwrite the saved
binding or continue polling a different listener generation.

The existing link TTL (five minutes), two-second polling, high-entropy
memory-only polling secret and one-use `web-` approval are unchanged. The owner
lookup response gains optional `reauth:{version:1,credential_id:"<id>",key_sha256:"<hash>"}`,
where the hash is base64url SHA-256 of the 65 public-key bytes. The CLI continues
to show origin, listener and scope and adds: “This browser may reconnect without
another approval until this authorization expires, for up to 12 hours.” Approval
binds the key before any credential is released. Denial grants no authority.
Owner-only `browsers.list` entries gain the same optional `reauth` object, so an
owner reviewing grants can tell which ones a browser can recover after reload.
`browsers.delete` ends recovery for that grant at once, as for any revocation.

`POST /v1/browser-links/{id}/poll` retains its existing request and response union.
For a key-bound approved grant, the existing `grant` object gains `reauth` with
the same four fields above. `id`, `token`, `scope`, `origin`, `listener` and
`expires_at` retain their names and meanings. Keyless grants omit `reauth`.
The approved record is eligible for proof immediately after approval, even if
the poll response was lost. First successful poll **or proof** marks it collected
and consumes the link; it must not later be revoked by uncollected-link cleanup.
An approved but uncollected grant still ends at its link's original deadline.
That deadline is enforced at the proof release point itself. The existing
cleanup of uncollected approvals runs only when another link is created, so a
proof path that consulted the binding alone could release an abandoned approval
for up to twelve hours; it must check the link deadline under the release lock
and end the grant there. Poll and proof share that lock: exactly one of them
collects. A poller whose link was collected by proof from another same-origin tab
receives the existing `410 link_gone`; for a key-bound candidate the client then
tries proof once before reporting a link failure, and never starts a new link
automatically.
Thus a reload during pending linking loses the polling secret but may recover a
just-approved grant by proof within that deadline. A never-approved/denied/expired
candidate cannot recover; the client offers a fresh explicit link, not automatic
owner approval or repeated link creation.
Uncollected-link timeout is reported as `reauth_failed`, not twelve-hour grant
expiry; its public binding may be discarded with that link.

### C3. Challenge/response, exact bytes and failures

Two new browser-only operations, `BrowserReauthChallenge` and
`BrowserReauthComplete`, join the minimal unauthenticated routes in the existing
browser authorization gate. They are available only where links are eligible;
all operational routes still require the bearer/ticket. Both use the existing
API error shape `{"code":"<stable_code>","error":"<text>"}`.

`POST /v1/browser-reauth/challenge`, without Authorization:

```json
{"version":1,"credential_id":"<id>","daemon_epoch":"<saved-epoch>","listener_epoch":"<saved-epoch>","client_nonce":"<32-byte-base64url>"}
```

201 response:

```json
{"version":1,"credential_id":"<id>","daemon_epoch":"<epoch>","listener_epoch":"<epoch>","origin":"http://127.0.0.1:3000","listener":"loopback","challenge_id":"<32-byte-base64url>","nonce":"<32-byte-base64url>","client_nonce":"<echo>","expires_at":"2026-11-12T12:00:30.000Z"}
```

Each request uses a fresh client nonce; each response uses independent random
challenge ID and nonce. Challenge lifetime is 30 seconds from creation, ended
by whichever of daemon monotonic time or the wall-clock `expires_at` passes
first, so a suspend cannot stretch it; `expires_at` is an RFC3339 UTC string with
exactly three fractional digits and `Z` for display/transcript. Epoch mismatch
against the receiving daemon/listener returns `409 daemon_restarted` or
`409 listener_changed` respectively (daemon check first), before lookup. These
public errors reveal no grant state. Otherwise return the same 201 shape for
known, unknown, expired or revoked IDs, including IDs belonging to another
allowed origin/listener. The challenge by itself confers no authority and
contains no grant metadata. Binding mismatches ultimately fail generically.

The signed message is the concatenation of the ASCII bytes
`awman.browser.reauth.v1` followed by a single zero byte, then the following
eleven strings in this exact order. Each string is encoded as a four-byte
unsigned big-endian UTF-8 byte length followed by those UTF-8 bytes:

1. `POST`
2. `/v1/browser-reauth/complete`
3. `credential_id`
4. `daemon_epoch`
5. `listener_epoch`
6. `origin`
7. `listener`
8. `challenge_id`
9. `nonce`
10. `client_nonce`
11. `expires_at`

The field values are the exact strings in the challenge response, without
normalization or parsing/reformatting the timestamp. This is not JSON signing.
For example a string `POST` contributes bytes `00 00 00 04 50 4f 53 54`.
Before signing, the client checks the response version, credential ID, both
epochs, exact `location.origin`, expected listener name, and its own fresh
client nonce against the selected local record/request. It also validates all
lengths/encodings and refuses additional/missing response fields for this version.
It signs only this typed transcript, never arbitrary server-supplied bytes.
`crypto.subtle.sign({name:"ECDSA",hash:"SHA-256"}, privateKey, message)` produces
a 64-byte signature, `r[32] || s[32]`, unsigned big-endian and zero padded.
Transmit unpadded base64url (86 characters). Rust must accept this raw format,
not ASN.1 DER, and accept both valid high-S and low-S signatures: nonce consumption,
not ECDSA signature normalization, prevents replay. Hash the message exactly once.

`POST /v1/browser-reauth/complete`, without Authorization:

```json
{"version":1,"challenge_id":"<id>","signature":"<64-byte-base64url>"}
```

200 response:

```json
{"grant":{"id":"<existing-grant-id>","token":"awb_<existing-secret>","scope":"engine","origin":"http://127.0.0.1:3000","listener":"loopback","expires_at":"<original-grant-expiry>","reauth":{"version":1,"credential_id":"<id>","daemon_epoch":"<epoch>","listener_epoch":"<epoch>"}}}
```

After outer gate, size and rate checks, the daemon atomically removes the
challenge on the first structurally valid complete request, including a malformed
signature string, wrong binding or wrong key; one concurrent request at most can
succeed. A structurally valid request has exactly the three specified fields,
version 1, a correctly encoded challenge ID and a string signature. Invalid
signature encoding consumes the challenge and returns 400; an invalid signature
value returns 401. No retry returns a cached 200.
It reconstructs the transcript from its stored challenge, verifies against the
originally approved public key, and then rechecks live grant, origin, listener,
epochs, absolute expiry and (for an uncollected approval) the link deadline at
the credential-release linearization point. When no binding exists for the
challenge's credential ID, verify against a fixed dummy public key so unknown
and wrong-key failures cost the same.
Revocation that wins this race prevents release; a later revocation immediately
invalidates even an already returned bearer and all tickets/sockets. Grant
mutation and proof completion must share this serialization boundary.
Challenge issuance, receipt and verification never extend grant or primary grace.

| Condition | HTTP/code | Exact API `error` text / retry |
| --- | --- | --- |
| Malformed DTO/key/encoding/version | 400 `invalid_request` | `Invalid browser reauthorization request.` No automatic retry. |
| Reused enrollment credential ID | 409 `credential_exists` | `Browser credential ID is already registered.` |
| Expected daemon epoch differs | 409 `daemon_restarted` | `Daemon generation changed.` Probe; require a fresh link. |
| Expected listener epoch differs | 409 `listener_changed` | `Listener generation changed.` Probe; require a fresh link. |
| Unknown/consumed/expired challenge, wrong key, missing binding, wrong origin/listener, unapproved candidate | 401 `reauth_failed` | `Browser reauthorization proof was not accepted.` No detail oracle. |
| Valid proof of an expired authorization | 403 `authorization_expired` | `Browser authorization expired.` Require a fresh link. |
| Valid proof of a revoked authorization | 403 `authorization_revoked` | `Browser authorization was revoked.` Require a fresh link. |
| Capacity/rate limit | 429 `rate_limited` | `Too many browser reauthorization requests.` Integer `Retry-After` seconds. |

Disallowed Origin still fails at the outer gate without readable CORS, not with
a revealing proof error. Allowed-origin mismatch, listener mismatch and wrong
key always get the same `reauth_failed`. An expired/revoked response is issued
only after a valid signature under the old binding. Tombstones retain public
key/binding, original expiry and first termination reason, never the bearer or
its hash, until original grant expiry plus 24 hours or daemon stop, whichever
comes first. Revoked stays revoked even if its original expiry later passes.
Unknown/purged records return generic failure, never a guessed reason.
The first termination reason maps to exactly one outcome: owner deletion and
origin removal give `authorization_revoked`; expiry gives
`authorization_expired`. Listener-wide endings and daemon stop keep no
tombstone, because the changed listener or daemon epoch already answers
`409 listener_changed` / `daemon_restarted` before lookup.

Limits are part of the interface: eight outstanding challenges per presented
credential ID (known or unknown, counted identically) and 256 per listener. At
the per-credential limit the daemon drops that ID's **oldest** outstanding
challenge and issues the new one; challenge creation is unauthenticated and
credential IDs are not secret, so refusing instead would let anyone who has
seen an ID hold all eight slots and block that browser's recovery. A tab whose
challenge was dropped gets `reauth_failed` and its one fresh retry. At the
listener limit refuse with 429/`Retry-After: 30`. Rate-limit challenge creation
to 60/minute per (listener, Origin) and completion to 120/minute per
(listener, Origin). There is **no per-credential proof-failure lockout**: a
P-256 signature cannot be guessed, so a lockout adds no protection and would
let a caller with an allowed Origin lock a known credential ID out of recovery.
Proof failures are counted only for diagnostics. Bound limiter maps and expire
idle entries; never create a bucket per attacker-selected ID. Reserve at most 4096 enrollment/binding/
tombstone slots per listener, reject additional link creation with 429 and a
60-second retry hint, and release slots on denial/pending timeout or tombstone
expiry. Do not evict live authorizations to admit a new link. Existing five
link-creations/minute and three-pending-link limits remain. Proof responses and
signatures are redacted like `BrowserSecret`, including library TRACE payloads.

A lost complete response can be recovered with a **new** challenge and signature.
The web client runs one recovery attempt per document at a time. It retries one
fresh challenge on `reauth_failed` (a stale tab may have outlived the challenge),
then shows unavailable; network failures use existing bounded reconnect backoff
and 429 honors `Retry-After`. A 401 from an operational route first clears only
the in-memory bearer, then probes and attempts proof once to classify loss. If
that proof succeeds and the returned bearer is refused again, stop: show
`authorization_unavailable` without a further cycle (the proof returns the same
bearer, so repeating cannot help). Do
not relabel every 1008/401 as revoked or delete a key on a transport failure.
Explicit close reasons `grant revoked`/`grant expired` can terminate immediately.
`origin removed` and `listener closed` also end that authorization immediately
with their specific C7 explanations; no proof can bypass the changed gate.

### C4. Stored data and cleanup

| Location | Exact allowed data | Cleanup / exclusions |
| --- | --- | --- |
| IndexedDB database `awman-browser-auth`, version 1, store `credentials`, key path `credentialId` | `{version:1, credentialId, privateKey:CryptoKey, publicKey:string, origin:string, daemonBase:string, listener:string, daemonEpoch:string, listenerEpoch:string, grantId:string|null, expiresAt:string|null, linkExpiresAt:string|null}`; publicKey is the raw-key base64url. Initial record has null grant/expiry/link-expiry until responses arrive. | Max eight records per page origin; explicit new linking replaces only a selected obsolete record. Record is atomically updated after poll/proof. Do not silently evict a still-valid credential. No general object serialization of grant/HTTP responses. |
| `sessionStorage` key `awman.web.resume` | Existing `{instanceId,frontendId,tabId,launchContext}` plus `{version:2,credentialId,daemonEpoch,listenerEpoch}`. Nullable tab/context remain opaque IDs. | Per-tab navigation hint, validated against recovered grant and current snapshot. Clear obsolete binding on expiry/revocation/restart, explicit forget, instance end or user leaving that instance. Legacy records without binding fields cannot auto-resume. |
| `sessionStorage` key `awman.web.view` | Existing bounded numeric panel sizes and allowlisted focus IDs | Presentation only. |
| Document memory | Bearer, polling secret, challenges, signatures, sockets, snapshots, dialogs, input, logs and terminal bytes | Discard on document destruction or authorization end. Zero references to bearer/proof as soon as practical; JavaScript GC is not secure erasure. |
| Daemon private file | Existing grant hash and metadata only | No private key, raw bearer, signature or pending challenge. Existing discard-on-open behavior remains. |
| Daemon memory | Public key binding, original bearer in secret wrapper, epochs, bounded challenges, resume ownership and public tombstones | Remove bearer/hash access and tickets at expiry/revocation; close watchers. Clear all on graceful stop or crash/start. Never restore browser authority from the old file. |

Neither `localStorage` nor cookies are used by this mechanism. No credentials,
keys or session content in URLs, DOM data attributes, hydration payloads, logs,
clipboard, analytics, public Worker requests/responses or build artifacts. The
link code/owner approval command remain intentionally visible and copyable;
polling secrets and bearer grants do not. No key/proof/bearer sharing through
BroadcastChannel, postMessage, a SharedWorker or service-worker messages.

IndexedDB content is untrusted input: same-origin script can rewrite it. On
every read validate the exact schema, field lengths and encodings and refuse
unknown fields; require `privateKey` to be a private ECDSA P-256 `CryptoKey`
with `extractable === false` and usages exactly `["sign"]`. Drop a record that
fails. Render stored strings only as text.

Read only records whose page origin and `daemonBase` equal the current trusted
configuration; a stored base is a comparison value, never a fetch destination.
Choose the record referenced by this tab's resume hint. Without a hint use a sole
matching record, or ask the user to select a saved authorization if several
match; never guess an instance from IDB or another tab. The picker may display
listener and expiry, not paths/session content. One tab's key deletion may
prevent another tab's future recovery, but it cannot revoke that other tab's
live bearer: distinguish local “Forget this browser” from owner-side revocation.
For immediate revocation present the existing `browsers.delete` owner command.

After verified expiry/revocation, changed daemon/listener generation, explicit
forget, or corrupt/unusable key, remove the affected IDB record and this tab's
bound resume hint. A failed deletion leaves it unusable in the document and
cannot restore server authority. On later load it is revalidated before use.
Also remove a record after two consecutive `reauth_failed` answers on fresh
challenges while the daemon is reachable and both epochs match: the daemon has
then stated it holds no usable binding, and bindings never return. The one
exception is a candidate still inside its known link deadline, which the owner
may yet approve. Without this rule a purged or never-approved record would fail
on every load and reappear in the saved-authorization choice. Before a new link,
remove records whose epochs differ from the probed daemon; if eight usable
records still remain, require the user to forget one rather than evicting.
Do not delete on timeout, permission denial, 429 or temporary disconnect. An
uncollected candidate is discarded after its link deadline; if that deadline is
unknown because creation's response was lost, try proof once before offering
new linking. A locally passed expiry is a UI hint only; a reachable daemon's
verified outcome is authoritative. Browser restart may recover authorization
within its original window; sessionStorage/grace may already be gone, so it
does not promise to recover a primary instance.

### C5. Instance resume, tabs and terminal ownership

Authorization and primary ownership are independent. Recovering a bearer never
creates an instance, selects a workspace, submits a command, answers a dialog,
or grants terminal input. Use the existing frontend ticket request
`{"route":"frontend","instance_id":"<saved-id>"}` and `Hello.resume` only after
the exact tab binding is validated. No changes to ticket wire shapes, ticket
30-second TTL, one-use semantics or resource checks are required.

The current socket adapter loses browser identity after upgrade (it passes a
boolean and a grant watch to the host). The daemon implementation must instead
pass the authenticated browser principal to a Layer 2 admission method. On a
successful initial browser primary admission, atomically record
`(instance_id, frontend_id) -> grant_id` beside that primary's lifecycle. A web
resume requires the **same live grant ID**, plus the existing instance/frontend
ID check, disconnected primary state and `now < disconnect + 60 seconds`.
Grant recovery preserves that ID. A newly approved grant, guessed ID or another
browser grant cannot resume it, even when resource visibility permits secondary
viewing. Enforce this on admission, not just ticket creation, with the existing
`primary_exists` refusal shape for an unauthorized resume. A connected primary
is never displaced. Admission and recording are atomic with lifecycle changes;
failure sends no Welcome and leaves no partial primary. Remove ownership when
the primary/instance ends; revocation disables resuming immediately. TUI/API-key
admission and secondary visibility rules remain unchanged.

Same-origin tabs may use the shared key to recover the same grant independently.
For cooperative duplicate-tab handling, before attempting a primary, acquire an
exclusive [Web Lock](https://www.w3.org/TR/web-locks/#api-lockmanager-request)
named `awman-primary:` plus the compact JSON array
`[daemonEpoch,listenerEpoch,instanceId]`. Hold it for the document's primary
connection and reconnects; never use `steal`. Acquire with `ifAvailable:true`.
A reloading tab can ask before the browser has released its previous document's
lock, so a tab holding a matching resume hint retries acquisition at the
existing 50 ms–1 second backoff for at most five seconds before giving up; an
immediate refusal would turn a routine reload into the other-primary state.
After that, failure shows the other-primary state and offers a secondary view
or a new instance. No unbounded queued takeover. Keep the lock while a
disconnected tab is trying to resume; release on explicit leave, terminal
authorization loss or document destruction. This lock is a UX coordination
device, not authorization. The explicit “Resume workspace” action therefore
does not depend on it: on that user gesture the tab asks the daemon for the
resume even when another document still holds the lock (for example a frozen
tab whose socket has died), and the daemon decides. Otherwise such a tab would
block recovery until the 60-second grace ran out. A tab that then finds itself
refused shows the other-primary state and does not retry on its own.
If the Lock API is unavailable, disable automatic primary restoration and offer
a secondary view or explicit “Resume workspace”; the daemon remains the arbiter.

An original reload may briefly race its old socket teardown. When the lock is
held but the daemon says `primary_exists`, retry with fresh tickets at the
existing 50 ms–1 second backoff for at most five seconds, then show the other-
primary state with a manual retry action. Never close another socket or promote
a secondary. Cloned sessionStorage carries the same hint; the lock and atomic
daemon admission allow at most one winner. If simultaneous reloads race after
both originals disappear, the first valid admission wins; the other is read-only.
The client cannot prove which identical browser tab was historically original.
An independent tab with no hint opens the picker and may explicitly attach
secondary or create its own instance. It cannot auto-select an existing primary.

Reload recovery proceeds: probe/permission/version checks → read matching key →
challenge/proof → validate grant → acquire primary lock → fresh ticket and saved
`Hello.resume` → Welcome and full snapshot → restore saved tab if it still exists.
If the tab closed, show the snapshot's active tab and C7's closed-tab notice. If
the instance is missing or grace has expired, show C7's ended state and the
picker; do not silently create another instance or retry Hello without `resume`.
The daemon's normal primary-exit policy after grace still decides running work's
fate. The grant may remain valid and be used for a new deliberate workspace.

During reconnect keep an already-loaded view marked disconnected/read-only;
after an actual reload there is no persisted content to display until snapshot.
Discard the outbound queue. Never replay unacknowledged commands, dialog answers,
paste, keystrokes, or terminal input. An HTTP mutation interrupted by reload is
also not retried automatically. A document restored from the back/forward cache
(`pageshow` with `persisted`) or resumed after being frozen still holds its old
memory: treat it as a reconnect, discard queues, open dialog forms and typed
secrets, check the bearer with the daemon and rebuild from a new snapshot
before enabling any mutation. The new snapshot is authoritative for running
commands and pending dialogs; first-answer-wins remains unchanged. Show the
generic restored notice in C7 because a reload loses in-memory acknowledgements.
It does not promise historical command output: output feeds are memory-only and
the frontend snapshot does not replay them. Terminal hub snapshots resync each
live terminal independently. Destroy old dialog forms and typed secrets; pending
dialogs come only from the new snapshot. Mobile panel switching has no command,
answer, dismissal or ownership side effect.

Terminal input grants belong to terminal connections, not the browser credential
or frontend resume ID. Reopen visible terminals with fresh terminal tickets,
receive hub snapshot/control state, and show “View only” until the hub confirms
this new socket holds input. Never replay a take-control request or persist its
grant. If input was lost, C7's terminal notice and explicit control action make
that loss visible. Recompute positive viewport dimensions after control is
confirmed and after reconnect; viewers send neither input nor resize. Refresh
does not silently steal another terminal holder's control.

### C6. Replay and threat boundary

An IDB metadata copy contains public identifiers and cannot authorize a request
without the key. A forged Origin header by a local process is insufficient
without a bearer or valid proof. Cross-origin pages cannot read the key, receive
permissive CORS, enroll outside the owner's decision, or use a different origin's
key even if it was copied. Epochs bind proof to one process/listener incarnation;
listener-name reuse is insufficient. Exact typed transcripts prevent cross-route,
cross-origin and cross-listener substitution. Consumed/expired nonces, signature
malleations, simultaneous submissions and lost-response retries cannot reuse a
proof. Server expiry and revocation are checked even after signature verification.
Public health epochs detect discontinuity; they do not authenticate a hostile
replacement daemon. The existing loopback transport trusts the local machine;
local traffic interception/port takeover is outside the protection of this proof.

XSS, a compromised app deployment/service worker or a privileged extension can
invoke the signing key and steal a recovered bearer. A non-extractable key is
not a defense against that threat; expiry, owner revocation, disabling the
listener/removing its origin, CSP, trusted shell updates and inert output
rendering remain necessary. The new recovery privilege lasts at most the same
approved window, never indefinitely. The daemon cannot attest that a supplied
public key was generated non-extractably; that is an obligation of the trusted
web client and its tests. Host/daemon compromise and malicious local owner are
outside this authorization boundary. Rate limits reduce but cannot eliminate
denial of service by an allowed-origin/local adversary.

Simpler mechanisms were considered. A bearer in `sessionStorage` or
`localStorage` (which the owner permits) would also survive reload with far less
protocol. It was not chosen because item 7 prefers a proof over a stored raw
grant, and because the key keeps any reusable secret string out of
script-readable storage, storage dumps, bug reports and storage audits; a
one-time read of storage yields nothing usable later. The margin is modest: it
does not help against script running in the origin, and not against a reader
of profile files (C1). Cookies were rejected: the page and the loopback daemon
are cross-site, so a cookie would depend on third-party cookie behavior and
would add ambient authority to every daemon request. Minting a new bearer per
proof instead of retaining the original was rejected as well: it would not
reduce exposure without rotation, and rotation would break independent tabs.

### C7. Exact recovery explanation text

These are client state keys and exact English explanation paragraphs, not new
engine frames. Preserve existing link/setup error text alongside these notices.
Link actions start only from a user gesture and display a new concrete owner
approval command. Read-only states retain navigation and disable mutation.

| State / evidence | Exact explanation | Actions |
| --- | --- | --- |
| `reauthorizing`: matching key and reachable daemon | `Reconnecting this browser using its saved authorization. No new link approval is needed.` | Cancel/recheck |
| `restored`: proof and snapshot succeeded | `Reconnected to your workspace. Commands and answers were not resent. Check the current status before repeating an action.` | Normal workspace controls |
| `authorization_expired`: verified expiry or `grant expired` close | `This browser's 12-hour authorization has expired. Connect this browser again and approve the new link command.` | Connect browser |
| `authorization_revoked`: verified revocation or `grant revoked` close | `This browser's access was revoked. Connect this browser again only if the owner wants to restore access.` | Connect browser |
| `daemon_restarted`: same configured daemon base, different daemon epoch | `The daemon has restarted since this browser was linked. Its previous authorization is no longer valid. Connect this browser again.` | Connect browser, workspace selection after linking |
| `daemon_stopped`: observed `daemon stopping` close, not merely a timeout | `The daemon is stopping or has stopped. Start it on this machine, then choose Recheck. You will need to link this browser again after it restarts.` | Start command, Recheck |
| `daemon_unreachable`: failed fetch/timeout, no positive stop evidence | `The daemon did not answer. It may be stopped, its listener may be off, or the browser may be blocking access. Check awman daemon status, then choose Recheck. Your saved authorization has not been discarded.` | Existing setup/permission repair, Recheck |
| `other_primary`: lock unavailable or persistent `primary_exists` | `Another browser tab or frontend holds primary control of this workspace. You can view it here, return to the primary tab, or open a new workspace.` | View only, Resume workspace (manual retry), New workspace |
| `listener_changed`: listener name or generation no longer matches | `The daemon listener has changed since this browser was linked. Connect this browser again on the intended listener.` | Recheck, Connect browser |
| `listener_closed`: observed `listener closed` close | `The daemon listener was disabled or changed, so this browser's authorization ended. Check the listener settings, then choose Recheck.` | Existing setup recipe, Recheck, new link once eligible |
| `origin_removed`: observed `origin removed` close | `This site's origin is no longer allowed by the daemon listener. Ask the owner to review the allowed origins, then choose Recheck.` | Existing origin repair guidance, Recheck, new link once allowed |
| `origin_mismatch`: locally detected binding mismatch | `This authorization belongs to a different site origin. Return to the original site or connect this browser here with a new approval.` | Connect browser on current allowed origin |
| `authorization_unavailable`: generic proof failure, lost/corrupt key or purged binding | `This browser could not recover its saved authorization. Connect this browser again and approve the new link command.` | Connect browser; keep technical reason secret-free |
| `reauth_unsupported`: capability/version mismatch | `This daemon does not support saved browser authorization for this app version. Update awman, then choose Recheck.` | Recheck, update guidance |
| `memory_only`: key persistence unavailable | `This browser cannot save its authorization key. You can connect for this page, but reloading will require another link approval.` | Connect for this page, Recheck |
| `resume_unavailable`: missing instance/expired primary grace | `Your browser is authorized, but the previous workspace connection can no longer be resumed. Choose a workspace to continue. No command has been restarted.` | Workspace picker |
| `tab_closed`: saved tab absent from snapshot | `The previously selected tab has closed. Showing the workspace's current tab.` | Normal navigation |
| `terminal_view_only`: new terminal socket lacks input grant | `This terminal is view only after reconnecting. Request control to type or resize it.` | Existing explicit control action |
| `rate_limited`: 429 | `Reconnection is temporarily rate limited. Waiting before trying again.` | Recheck after Retry-After |

Permission-denied, engine-off, setup, link-denied/expired and transport-version
states retain their existing specific explanations. A CORS-hidden origin refusal
is indistinguishable from some network failures in JavaScript: show the existing
permission/blocked state when observable, otherwise `daemon_unreachable`, never
invent “revoked” or “restarted.” A different origin normally sees no old IDB at
all and enters first-time linking. Once a restart is observed, clear the old hint
before workspace lookup; never restore a coincidentally reused ID. A timeout
after page reload cannot establish a historical stop, so it uses unreachable.

### C8. Service worker and acceptance gates

`web/src/sw/policy.ts` and `sw.ts` keep their shell-only precache. They must ignore
every daemon request and `/v1/*` even on the same origin, all POSTs, authorization
headers and auth-like query strings. No auth responses in HTTP/Cache Storage,
no IndexedDB access or key/signing broker in the worker, no background sync,
no session-bearing messages and no changes to CSP or connect-src. Updating the
shell does not delete credential IDB; the reloaded compatible document performs
fresh proof. Incompatible schema/protocol fails closed with update/relink copy.
This is a code policy, not a claim that a compromised worker lacks the browser
privileges to access same-origin storage. Cloudflare continues to serve only the
static shell; auth traffic and credentials never go through it.

Before implementation is accepted, Rust and real-browser tests must jointly
prove: exact transcript/signature interoperability; valid reload without owner
approval; unchanged absolute expiry and grant ID; revocation and restart; wrong
origin/listener/key; nonce replay including concurrent and malleated proofs;
bounded tables; two tabs recovering without invalidating each other; grant-bound
primary resume and grace; running command/pending dialog refresh without replay;
fresh terminal ownership/geometry; storage eviction/failure; and shell update
with no secrets/session content in caches. Inspect IndexedDB to allow exactly
C4's records and verify key export fails. Allowlist that one persistence module
in existing lint/build storage audits rather than disabling them globally.
Detailed implementation ownership and test cases are in the workflow handoff
`/awman/context/workflow/reauth-contract.md`.

## Edge Case Considerations

- Very long paths, tab labels, command output, localized text, validation errors and multiple simultaneous agent cards must wrap or scroll within their own region without forcing horizontal page overflow or button collisions. Use grid/flex minimum sizing and contained overflow deliberately.
- Reduced transparency, missing `backdrop-filter`, dark mode, browser zoom and reduced motion must retain contrast and focus visibility.
- A secondary or disconnected frontend must still expose context while disabling engine-mutating actions; mobile panel switching must not answer or dismiss engine dialogs.
- Refresh during a running command, terminal session, pending dialog or primary reconnect grace must not duplicate a submission, switch to the wrong instance or drop terminal control silently. Multiple tabs, an expired authorization, explicit revocation and a stopped or restarted daemon need distinct states.
- The visual redesign must not change WebSocket frames, session state, CSP or browser cache boundaries. The authorization-continuity change requires an explicit update to the 0159 storage/security contract rather than an accidental exception.

## Test Considerations

- Run web typecheck, lint, unit tests, acceptance matrix, production build and CSP audit. Audit the action matrix again after implementation; every TUI capability available to the web listener must have a reachable web flow.
- Add browser layout checks at 1280×720, 1440×900, 1024×768, 768×1024 and 390×844 for connect, picker, active workspace, concurrent agents, long output and open dialogs. Assert no page-level horizontal overflow, overlap, hidden primary action or unusably narrow terminal. Exercise dark mode, 200% zoom, reduced motion, keyboard navigation and touch targets. Keep screenshots synthetic and free of session secrets.
- Use a real browser and terminal transport to resize the window and change layout/panels, then assert the holder sends the expected debounced resize frame, the daemon PTY receives the new dimensions and the agent TUI redraws. Verify viewers do not send resize and reconnect recomputes dimensions.
- Test refresh with a still-valid authorization and running session: no new `awman link` command, same instance/tab restored, no duplicate command or dialog answer. Test grant expiry, revocation, daemon restart, multiple tabs and origin mismatch. Inspect browser storage and service-worker caches for unintended credentials or session content.

## Codebase Integration

- Presentation remains in `web/src/app/globals.css`, `web/src/lib/shell/` and the existing React screen components. Engine state and outbound frame contracts remain in `web/src/lib/client/`; terminal sizing crosses the existing client and terminal-hub transport deliberately.
- Follow the Layer 3 frontend boundary in `aspec/architecture/2026-grand-architecture.md`. A daemon-side reauthorization endpoint or protocol change, if needed, belongs in the existing browser authorization layer and requires focused security tests, not a UI-only workaround.
- Keep all dependencies pinned and production styling compatible with `style-src 'self'`.

## Documentation

- Update `docs/20-webapp.md` for the new navigation and refresh behavior, including when relinking is still required. Keep visual implementation details and security analysis in this item and code, not the user guide.
