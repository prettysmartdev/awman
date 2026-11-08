# Work Item: Feature

Title: webapp v0
Issue: issuelink

Depends on: 0139–0142 (unified daemon API, terminal hub, engine and frontend protocol), 0143 (worker views), and later TUI/API features this webapp presents. Plan against their completed contracts; reconcile changes before implementation.

## Summary:
- Build a Next.js, installable, offline-capable PWA at https://awman.prettysmart.dev. Cloudflare serves only the app shell. The user's local awman daemon runs every session, agent, command, workflow and terminal; there is no cloud relay, daemon or agent.
- Recreate the TUI's features and workflow behavior with responsive web controls and applicable keyboard shortcuts. The webapp is a presentation frontend for the daemon engine, not another implementation of awman business logic.
- Keep session content and daemon traffic between the browser and the user's machine. The serving Worker records only aggregate anonymous counts from the initial document request, with no client telemetry or per-user identifiers.
- A browser cannot use 0140's owner-only Unix socket/named pipe, and a phone's loopback address is not the desktop's. This item includes a separately reviewed, paired browser gateway to the local daemon. It may be split into security/transport, web UI and deployment work items, but all gates must pass before webapp v0 is complete.

## User Stories

### User Story 1:
As a: developer using awman

I want to:
open local sessions in a browser and perform the same tab, command, workflow, config and agent actions as in the TUI

So I can:
move between terminal and browser without changing how my work behaves.

### User Story 2:
As a: mobile user

I want to:
pair my phone with my daemon on the same trusted local network and control interactive agent terminals with touch or a keyboard

So I can:
use the full awman workflow away from my desktop screen without moving agent work to the cloud.

### User Story 3:
As a: privacy-conscious user

I want to:
install and reopen the webapp when Cloudflare is unavailable, while keeping my session data local

So I can:
continue working with a reachable daemon and know that the hosted service records only anonymous page-load totals.

## Implementation Details:

### 1. Contracts and acceptance inventory
- Start from the completed 0139–0142 contracts: /v1/engine/instances, awman.frontend.v1, /v1/terminals, awman.terminal.v1, the shared command catalogue, typed dialogs, instance snapshots/deltas and terminal input grants. Use 0143's worker and squad resources where TUI screens expose them. Feature detection uses /v1/status capabilities and protocol versions; an unknown required capability blocks that control with an upgrade message.
- Make a screen/action matrix from docs/02-using-the-tui.md, src/frontend/tui/{app,key_handler,keymap,dialogs,workflow_view,container_view,git_sidebar,command_box}.rs and the completed aspec. Cover startup/ready, repo selection, tabs, command box/autocomplete, execution log, agent status, git sidebar, config, squad, workflow overview/control/approvals, parallel containers, dialogs, quit/cancel and every agent terminal. Record the daemon operation and browser interaction for each row; automate parity against this matrix. TUI features added before release join it.
- The engine remains authoritative for command parsing, defaults, state transitions, validation, approvals, lifecycle and errors. The webapp stores only view state such as focus, scroll and panel sizes.

### 2. Browser gateway and launch context
- Add a browser gateway as a separate, opt-in daemon-facing adapter. By default it binds only 127.0.0.1/[::1]; it forwards a fixed, typed allowlist of engine, terminal, status and TUI-required feature operations to the owner-only local listener. It is not a generic HTTP proxy and does not enable engine on 0140's ordinary network listener. Layer 2 authorizes browser actions and terminal grants; Layer 3 handles HTTP/WebSocket transport. Advertise web_frontend only when this gateway and browser-compatible protocol are ready.
- Pairing starts with an explicit local owner action (awman web pair or equivalent owner-only daemon command): show the target origin, device scope and an expiring one-use code/QR locally. The browser sends that code only to the gateway; the daemon issues a revocable, short-lived, origin-bound browser grant. Store grants only in owner-private local storage; keep live tokens in browser memory, never in URL, cookies, localStorage, service-worker caches, logs or Cloudflare responses. Re-pair after browser restart if needed. Provide local grant listing and revocation; rate-limit pairing/authentication failures.
- Gate every request and WebSocket upgrade on the exact https://awman.prettysmart.dev Origin, expected literal loopback Host/address (or an explicitly configured LAN host), valid browser grant, route/method allowlist, resource ownership and bounded message sizes. Reject missing/null browser origins, DNS rebinding, redirects to another host, wildcard CORS, credentialed cookies, unpaired WebSockets and cross-site form submissions. CORS permits only the production origin and a separately configured development origin. Origin is not authentication; the grant is required too. Revocation or daemon lock/stop closes live connections.
- Browser WebSocket APIs cannot set arbitrary Authorization headers. Specify a one-use, short-lived upgrade ticket obtained through authenticated, non-cacheable fetch and sent as a validated WebSocket subprotocol; never put credentials in query strings. Bind the ticket to grant, route, origin, expiry and single use. The gateway consumes the ticket without echoing it and selects only the actual awman.frontend.v1 or awman.terminal.v1 protocol. Reconnect obtains a new ticket and resynchronizes from snapshot; do not replay an unacknowledged command or answer blindly.
- The 0142 TUI Hello.env contains shell secrets and a browser cannot supply a faithful shell environment. Extend the frontend handshake with a Web kind and daemon-owned opaque launch-context ID. Create launch contexts through an owner-side CLI action from the desired cwd (or a locally configured, canonical allowlisted project root); keep environment/secrets on the host and resolve paths/config through the existing engine code. The browser may select only authorized contexts and never submit arbitrary env, executable path, host command or mount root. Preserve existing approval prompts for host-sensitive operations. Specify default context and new-tab project selection.
- Mobile access requires a separate explicit LAN option for this gateway, with a browser-trusted TLS certificate, WSS, private-network bind/firewall restriction, per-device pairing and revocation. No plaintext LAN credentials, public listener, Cloudflare Tunnel or relay. If a secure LAN endpoint is absent, show setup guidance; do not fall back to cloud transport. Test from a phone-sized browser on another device.

### 3. Next.js UI and terminal presentation
- Put the web project in a dedicated web/ package. Build a static, client-rendered Next.js application with no session-dependent SSR, Server Actions, API routes or server-side daemon fetches. Host assets through a Cloudflare Worker at the required domain. The Worker serves the app shell only; browser-to-daemon traffic goes directly to the gateway. Audit built output and network traffic for accidental cloud requests from session flows.
- Use one typed browser client for awman.frontend.v1 and awman.terminal.v1. Apply InstanceSnapshot then ordered InstanceDeltas; handle sequence gaps, lagged, chunked snapshots, disconnect/reconnect, primary/secondary ownership, first-answer-wins dialogs and terminal-control changes. Follow 0142 instance exit/close semantics; refresh must not duplicate submission or unexpectedly close live work.
- Build responsive layouts from the action matrix: desktop tab bar, execution area, command composer, workflow and git panels; mobile bottom navigation/sheets with touch targets, readable status, safe-area insets and persistent access to the active agent. Keyboard shortcuts use a centralized map derived from TUI behavior (for example Ctrl+T/A/D/G/M/O/W, Ctrl+\, Ctrl+, and context-sensitive Ctrl+C/Esc). Respect browser/OS-reserved combinations and focused text inputs; show visible controls and an in-app shortcut list for unavailable keys.
- Render agent TUIs with a browser terminal emulator fed only by terminal-hub snapshot/output frames, preserving colors, cursor, alternate screen, selection, paste, mouse and keyboard input, resize, parallel terminals and reconnect snapshots. Encode input as the protocol expects; resize only while holding the grant, debounce resize and show read-only/take-control state. On touch devices provide a control-key/escape toolbar and paste. Treat agent output, ANSI/OSC, titles, URLs, repo files and command output as untrusted text: no raw HTML, script execution, automatic URL navigation, clipboard writes or download behavior from terminal escape sequences. Only user gestures may open vetted links or write clipboard.
- Preserve dialog order, confirmation wording and outcomes from the engine; render all 0142 DialogRequest variants, including secret input, workflow controls, setup, mount-scope and config editing. Secret fields never enter analytics, persisted browser state or debug traces. A web-native TerminalRelease view stays inside the hub terminal rather than launching a process on the browser device.

### 4. Offline behavior and Cloudflare usage counts
- Ship a versioned manifest, icons and narrowly scoped service worker that precaches only app shell, fonts and static assets after first successful load. Update atomically so an old shell cannot run with incompatible chunks. Never cache gateway responses, session snapshots, terminal bytes, credentials or user-entered commands in Cache Storage, IndexedDB, localStorage, HTTP caches or the service worker. Mark all gateway data Cache-Control: no-store. When Cloudflare is unavailable, the installed shell opens and may reconnect directly to a reachable local/LAN daemon; explain that first-time offline installation is impossible.
- The Worker increments a stored daily page-load count only for the initial top-level HTML document request. Use an atomic aggregate counter (for example a Durable Object or D1 atomic increment) and retain only {UTC day, count}. It may inspect the request solely to identify a document navigation; it must not store or hash IP address, user agent, referer, URL parameters, cookies, identifiers or fine-grained timestamps. Counts represent page loads, not unique people. No browser analytics, beacons, tracking pixels, telemetry endpoint, service-worker background sync or client-to-Worker usage messages. Asset/manifest requests, fully offline loads, daemon traffic and health checks generate no extra records. Provide an owner-only aggregate review/export path returning daily totals; set a retention period and disable application-controlled request/analytics logs that retain identifying metadata. Document the remaining Cloudflare platform request-metadata boundary honestly.
- Set restrictive CSP (self-hosted, pinned assets only; no third-party scripts, unsafe inline scripts, eval or cross-origin embeds), Referrer-Policy no-referrer, appropriate Permissions-Policy and build integrity checks. Predeclare only the fixed loopback and LAN gateway hostnames/ports in connect-src; reject custom endpoints that cannot satisfy that policy. Verify the production Next.js output works without weakening CSP. Serve no session data in HTML, hydration payloads, redirects or error pages. Pin and review web dependencies and deployment changes.

## Edge Case Considerations:
- **Browser network permissions and mixed content:** Modern browsers may ask before a public HTTPS origin reaches loopback or LAN. Support the permission prompt from a user gesture, standard CORS, denied/revoked states and browser-specific failures. Verify exact HTTP(S)/WS(S) combinations on supported browsers; never weaken gateway authentication to work around a browser block. See https://developer.chrome.com/docs/modern-web-guidance/guides/security/local-network-access.
- **Daemon absent, engine off, version mismatch or LAN lost:** Distinguish these states, offer local setup/repair guidance and keep cached shell usable. A daemon restart can invalidate grants, instance IDs or terminal IDs; re-authenticate and resync without attaching to the wrong instance.
- **Multiple tabs/devices and lifecycle:** One primary per instance per 0142; another browser may attach as secondary or create its own instance. Closing/reloading a browser tab follows an explicit web disconnect policy with reconnect grace, then 0142 primary exit behavior. Make the effect visible before destructive close. Backgrounded mobile tabs may suspend sockets without user intent. Competing dialog answers remain first-wins; late answers receive DialogClosed.
- **Offline shell with incompatible daemon:** Show a version error and route to update when Cloudflare returns, without sending commands through an unverified protocol. Cached UI never treats stale state as authority.
- **Resource limits:** Bound terminal scrollback, incoming frames, queues, tab count and render work; use snapshots after lag and virtualize long logs/lists. Malformed frames, rapid resize, huge paste and slow devices fail cleanly without unbounded memory or leaked input grants.
- **Security boundary:** Hostile pages, malicious extensions, compromised served JavaScript, untrusted repo text/agent output, forged DNS/Host/Origin and hostile LAN peers are explicit threats. Origin/CORS alone cannot protect the daemon. Pairing, grants, route scoping, LAN TLS, CSP, output sanitization and approvals all need independent review. A compromised authorized origin remains high impact; deployment compromise response must revoke grants and disable the gateway, with no claim of absolute prevention.

## Test Considerations:
- Use the TUI action matrix as an acceptance suite: for identical scripted user actions, compare engine-side trace, state transitions, dialogs, commands, terminal ownership and quit/cancel outcomes between daemon-backed TUI and webapp. Cover all screens at desktop and mobile widths, touch and keyboard input, browser-reserved shortcuts, accessibility focus/labels and reduced motion.
- Integration tests with a real local daemon and browser verify pairing/revoke/expiry, launch context, command submission, workflow step approval, config edit, squad view, agent PTY input/output, alternate screen, mouse/paste/resize, parallel terminals, read-only control, lagged snapshots, reconnect and multiple frontends. Repeat mobile path from a separate LAN device with TLS; run Chromium and at least one other major browser engine.
- Adversarial gateway tests reject unpaired/wrong-origin requests, forged Host/DNS rebinding, CSRF forms, wildcard/preflight leakage, replayed upgrade tickets, malformed/oversized frames, unauthorized instance/terminal IDs, traversal, redirects and non-allowlisted routes. Fuzz terminal/ANSI/OSC and rich text rendering for script, clipboard and navigation injection. Verify browser-controlled env/host commands and protected mounts cannot cross the bridge.
- Offline/PWA tests install after first load, disconnect Cloudflare while keeping daemon reachable, launch and work normally, then update the shell atomically. Verify no session bytes, secrets or grants appear in browser persistent storage, service-worker caches, public Worker requests/responses or built artifacts; test an incompatible cached shell.
- Worker tests assert exactly one atomic daily increment per initial HTML request, none for assets, API-like paths or offline loads, and persisted rows contain only day/count. Review deployment config/logs for unintended identifying fields. Record frame latency, terminal throughput, reconnect time and mobile memory baselines; fail release on material regressions against the daemon TUI baseline.

## Codebase Integration:
- Follow aspec/architecture/2026-grand-architecture.md: Layer 0 owns wire types/capabilities, Layer 1 terminal mechanics, Layer 2 instance/launch-context policy and browser authorization, and Layer 3 gateway/presentation adapters. Reuse daemon routes/protocols rather than inventing web-only business operations.
- Keep src/frontend/tui/ as behavioral reference and add the web adapter alongside src/frontend/api/ without changing TUI semantics. Coordinate 0142 Hello extension, 0141 input grants, 0139 capability naming and 0140 local/network exposure rules in their shared modules. Browser grants are distinct from owner socket identity and ordinary remote API keys.
- Keep web build/deploy isolated from Rust binary release, with pinned dependencies, reproducible static output, CI type/lint/build, browser smoke/security tests and reviewed Cloudflare Worker configuration. Use current Cloudflare guidance at https://developers.cloudflare.com/workers/framework-guides/web-apps/nextjs/ and https://developers.cloudflare.com/workers/static-assets/get-started/ to choose the simplest supported static-client adapter.

## Documentation

After implementation is complete, update user-facing documentation in docs/ to reflect the current state of the tool:

- Add a webapp user guide covering install, pairing, local/LAN prerequisites, mobile use, shortcut differences, offline behavior, privacy counts, revocation and troubleshooting.
- Update docs/02-using-the-tui.md, docs/16-daemon.md, docs/07-configuration.md and docs/04-security-and-isolation.md where shared behavior or the browser gateway changes the user's choices.
- Keep protocol, threat model and deployment detail in this work item or code comments, not in published user docs; do not create a work-item-specific guide.
