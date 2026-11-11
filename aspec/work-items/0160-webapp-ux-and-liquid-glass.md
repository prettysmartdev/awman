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
