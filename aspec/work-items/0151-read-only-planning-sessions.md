# Work Item: Feature

Title: Read-only workspace for `exec prompt`
Issue: issuelink

Depends on: 0143 (completed daemon block).

## Summary:
- `exec prompt` runs in the session directory with no worktree. With the API's default `--yolo`, the agent can modify that directory. Clients that use a long-lived checkout for a planning or review agent must reset it (`git reset --hard`) before every call and hope nothing important was written.
- Add `--read-only-workspace`: the workspace is mounted `:ro`. Durable writes go to the context (0146) or outputs (0137) directories.

## User Stories

### User Story 1:
As a: API client running a planning agent that should read code but never change it

I want to:
`exec prompt --read-only-workspace ...`

So I can:
reuse one checkout safely, and run several planning prompts against it at once

## Implementation Details:
- New flag on `exec prompt` (and `exec workflow` agent steps, for review-only steps), mapped to a `:ro` workspace mount in every runtime. It is API-allowed.
- The system prompt gains one line saying the workspace is read-only and naming the writable locations (`/awman/output`, mounted contexts).
- Agents that need a scratch directory get a tmpfs `/tmp`.

## Edge Case Considerations:
- Agents that write caches into the workspace (e.g. `.claude/` state): point them at the agent home, which stays writable.
- Runtimes that cannot mount read-only: fail at preflight, never fall back to rw silently.

## Test Considerations:
- Write attempt fails inside the container on each runtime; context and output stay writable.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 0 owns the workspace intent and mount spec; Layer 1 enforces read-only mounts and writable scratch in every runtime; Layer 2 owns the catalogue flag, preflight and prompt policy; Layer 3 only collects the option and renders the result. The same Layer 2 path must apply in builtin, daemon, CLI, TUI and API frontends; no runtime may silently turn read-only into read-write.

## Documentation
- `docs/03-agent-sessions.md`, `docs/14-command-reference.md`.
