# Work Item: Feature

Title: `init` over the API
Issue: issuelink

## Summary:
- Dynamic workflows refuse any agent that has no `.awman/Dockerfile.<agent>` in the project (`workflow_preflight.rs`: "workflow.toml references agents with no Dockerfile in the project"). Only `awman init` writes those files, and `init` is not API-allowed (`catalogue/core.rs`: `api_allowed: false`). An API client given a repository awman has never seen cannot run a dynamic workflow on it. The leader burns its three repair attempts on a workflow that can never validate.
- Make `init` API-allowed in local sessions, answered with the headless defaults the CLI already uses when stdin is not a terminal (keep an existing `Dockerfile.dev` and `aspec/`, skip the agent audit, no work-items setup). Better still, have dynamic mode's preflight provision a missing `.awman/Dockerfile.<agent>` for first-party agents itself, the way it already builds a missing agent image.

## User Stories

### User Story 1:
As a: API client (infact) running work on teams' repositories

I want to:
`POST /v1/commands {"subcommand":"init","args":["--agent","claude"]}` in a local session, or have `exec workflow --dynamic` set up a missing agent Dockerfile

So I can:
run a team's first node without shelling out to the `awman` CLI on the API server's host

## Implementation Details:
- Mark `INIT` `api_allowed: true`. The API frontend answers `InitFrontend`'s questions with `HeadlessDefaults`, exactly as the CLI does for a non-TTY stdin. `--aspec` stays available.
- Init's image build reports through the command's event stream like `ready` does.
- Alternative: in `workflow_preflight`, when a referenced first-party agent has no project Dockerfile, run `provision_agent_dockerfile` (the step init uses) before failing, and tell the leader which agents are available before it designs the workflow, so a repair attempt is never spent on a missing file.

## Edge Case Considerations:
- Remote sessions: init writes into the session clone; the client decides whether to commit it.
- Custom agents whose Dockerfile render fails: fail the command with init's error, as the CLI does.

## Test Considerations:
- API `init` in a local session on an uninitialised repo writes `.awman/config.json` and `.awman/Dockerfile.claude` and asks no questions.
- A dynamic workflow on an uninitialised repo succeeds (alternative) or fails at preflight before the leader runs.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/14-command-reference.md` (API-allowed commands), the API guide.

## infact workaround this removes
- `Workspace::ensure_awman_init` (`crates/infact-server/src/adapters/workspaces.rs`) runs `awman init --agent <agent>` with the CLI in the leased workspace before a node run, and commits the result on the run branch. It needs the `awman` binary and the API server's `AWMAN_CONFIG_HOME` on infact's host (`--awman-bin`, `--awman-config-home`).
