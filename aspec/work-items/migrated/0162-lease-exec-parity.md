# Work Item: Feature

Title: Lease exec parity: per-item secrets, reviewed overlays, prerequisite choices and dynamic-workflow config over WSP
Issue: issuelink

Depends on: 0143 (WSP v1, lease execution and `policy.rs`), 0145 (per-command `secrets`, `env(NAME)` delivery and event-bus redaction), 0146 (`WorkItem.context`, which owns the `context` overlay kind for leases). Slots after 0161 in awman's order.

## Summary:
- Push commands and leases run the same engine, but a work server can tell a lease far less:
  - 0143 decodes `exec` with `deny_unknown_fields` and refuses every overlay a work item sends (`src/command/worker/policy.rs`: "lease-supplied overlays are not supported; configure reviewed overlays on the worker").
  - `exec` has no prerequisite or dynamic-workflow field.
  - 0145 adds `secrets` and `env(NAME)` to `POST /v1/worker/commands` and `/v1/worker/runs` only. For leases, it adds only Git transport and retry.

  A push client can set secrets, environment, skills, commit identity and workflow config per command. A work server can set none of these per work item.
- One worker serves many projects, so worker-wide config cannot stand in for per-item settings. Today a multi-team work server has to:
  - commit tooling config into each team's repository to set the commit identity;
  - give each worker one long-lived token instead of one per run;
  - do without skill overlays.
- Add four `exec` fields, each behind its own pull capability, with the worker owner keeping the final say:
  - per-item `secrets` with reviewed `env(NAME)` overlays;
  - reviewed `skill(name)` overlays for skills installed on the worker;
  - per-item prerequisite choices;
  - per-item dynamic-workflow config.

  The secret lifetime and redaction come from 0145, and the prerequisite widening rule is the one push uses, so one code path serves both.
- Also close two small discoverability gaps:
  - push sessions get a `workflow_prerequisites` capability: `/v1/status` has nothing that says the prerequisite flags, reports and failure stage are supported;
  - 0143's text says a refused link answers `403 access_denied`, but the `awman-wsp` mock server answers `400`.

## User Stories

### User Story 1:
As a: work server operator

I want to:
send a short-lived per-run token as `exec.secrets.GATEWAY_TOKEN` with each work item, and have every step container read it through `env(GATEWAY_TOKEN)`

So I can:
meter and revoke each run's access without configuring every worker

### User Story 2:
As a: work server operator

I want to:
send `exec.prerequisites: {git_identity: {name: "Lantern", email: "lantern@…"}, ssh: "deny", github_token: "deny"}` with a workflow item

So I can:
attribute agent commits to the project without committing awman config into the team's repository

### User Story 3:
As a: worker owner

I want to:
set `workServers.<name>.allowSkills: ["team-planner"]` and `allowEnv: ["TEST_TOKEN"]`

So I can:
let a server request those, and only those, without letting it mount arbitrary paths or read the rest of my environment

### User Story 4:
As a: push API client

I want to:
check `workflow_prerequisites` in `/v1/status` `capabilities`

So I can:
refuse an unsupported daemon before sending `--prerequisite-*` flags

## Implementation Details:

### Secrets and `env(NAME)` (`lease_secrets`)
- Add `exec.secrets: {NAME: value}` to `awman_wsp::Exec`. The limits are 0145's: at most 32 secrets of 8 KiB each, with names matching `^[A-Z_][A-Z0-9_]*$`. Decode it into 0145's Layer 0 secret type, never a plain `String`, so it cannot reach `Debug` output, the lease store or the job store.
- Values live in the runner's memory for the lease only. They are never in argv, `.git/config` or the persisted lease record. 0145's event-bus redaction applies to every event the worker posts, and to the WSP result's `final_message` and `failure.message`. A worker restart loses them, so the restarted lease is released with `error` and code `credentials_lost` before anything re-runs, as for push.
- `exec.overlays` accepts `env(NAME)` instead of refusing it. The value comes from `exec.secrets.NAME` when present. Otherwise it comes from the worker's environment, but only when `NAME` is in `workServers.<name>.allowEnv` (default empty). If neither applies, the lease is released `ineligible` and the reason names the variable. Inject the value into every step container of a workflow run (leader, steps, teardown), not only the leader.

### Reviewed skills (`lease_skills`)
- `exec.overlays` accepts `skill(name)` for a skill installed on the worker that is either in the default skill library or listed in `workServers.<name>.allowSkills` (default empty). Mount it the same way as a worker-configured skill overlay.
- At registration the worker reports each skill it would accept as the label `awman/skill.<name>=true`, so a server can match on it through affinity. A skill installed after registration needs a re-register, the same as any label change. Requesting a skill the worker would not accept releases the lease `ineligible`, naming the skill.
- Every other overlay kind (`dir`, `ssh`, …) stays refused `ineligible`, and the reason names the kind. A `context` overlay keeps 0146's meaning: leases get context through `WorkItem.context`, not an overlay.

### Prerequisite choices (`lease_prerequisites`)
- Add `exec.prerequisites` for workflow items: `{git_identity: {name, email} | null, ssh: "deny" | "allow", github_token: "deny" | "allow"}`. It means the same as `--prerequisite-identity`, `--prerequisite-ssh` and `--prerequisite-token`. The worker resolves it with the same Layer 2 code as push. A choice that widens what the worker's own `workflows.prerequisites` policy allows (including `allowApiClientGrants`) is released `ineligible` with code `prerequisite_widening_refused`, the lease form of the push error.
- The workflow's prerequisite report is posted as the usual event. On a prompt item `exec.prerequisites` is invalid, because `exec prompt` has no prerequisite flags.
- With no `exec.prerequisites`, behavior is unchanged: the worker's config decides, and the fallback identity is `awman <awman@localhost>`.

### Dynamic-workflow config (`lease_workflows_dynamic`)
- Add `exec.workflows_dynamic` for workflow items with `dynamic: true`: `{agents_to_models?, default_leader?, guidance?}`. Its meaning and validation are those of the `workflows.dynamic` config keys. It overrides the worker's and the repository's config for this item only. Lease provisioning (0143) already creates the Dockerfiles that the named first-party agents need. An unknown custom agent fails preflight before the leader runs, as in 0148.

### Advertising and conformance
- Add the four names to `PULL_CAPABILITIES`. Each is advertised in `/v1/status` and in `register.features` only while that path works for the worker's runtime; for example, `lease_skills` is not advertised on a runtime that cannot mount skills. A worker without the feature releases an item that uses the field `ineligible`; it never ignores the field. `deny_unknown_fields` stays, so an older worker rejects the field instead of silently ignoring it.
- Add `workflow_prerequisites` to `CAPABILITIES` once push accepts the four `--prerequisite-*` flags, posts the report event and uses the `workflow_prerequisites` failure stage, all of which `nextgen` already does.
- Make the `awman-wsp` mock answer a refused link with `403 access_denied`, matching 0143 and `docs/17`. Add a conformance case.

## Edge Case Considerations:
- 0145's redaction does not cover a secret value that also appears in a file the agent writes to `/awman/output`, or in terminal bytes. Document this for work server authors.
- A secret and an allowed worker variable with the same name: the secret wins, as on push.
- A secret named in `exec.secrets` but not requested by any `env(NAME)` is accepted and never injected. It is still redacted, so a value the server sent never leaks back.
- `exec.workflows_dynamic` on a non-dynamic workflow item is invalid.
- `remote_input` leases (0141) can show secret values typed or echoed in a terminal. Nothing new here, but document it next to the redaction limits.

## Test Considerations:
- `awman-wsp` conformance: each new field round-trips; a worker without the feature releases `ineligible`; unknown overlay kinds and unknown `exec` fields still fail closed; a refused link answers 403.
- Worker end to end, against the mock server:
  - a secret reaches every step container through `env(NAME)` and appears in no posted event, the result, the lease store, `.git/config` or `ps`;
  - an allowed worker variable is injected and a disallowed one is refused with its name;
  - `skill(name)` mounts an installed, allowed skill and refuses a missing or unlisted one;
  - the `awman/skill.*` labels are reported;
  - the commit author follows `exec.prerequisites`, and a widening request is refused `prerequisite_widening_refused`;
  - `exec.workflows_dynamic` changes the leader and mapping for one item only;
  - a restart mid-lease releases `credentials_lost`.
- `/v1/status`: `workflow_prerequisites` is present, and each `lease_*` name is present only while its pull worker reports it.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md` and `aspec/architecture/security.md`:
  - Layer 0: the new WSP types in `crates/awman-wsp` and the secret and name types reused from 0145.
  - Layer 1: secret lifetime, injection and redaction reuse 0145's paths unchanged.
  - Layer 2: overlay review, `allowEnv`/`allowSkills`, prerequisite widening and dynamic-config precedence live in `src/command/worker/policy.rs` next to today's refusal, and call the same functions push uses. The capability lists stay in `src/command/daemon_api/capabilities.rs`.
- No second redaction or prerequisite path for leases.

## Documentation
- `docs/17-workers-and-work-servers.md`: the new `exec` fields, `allowEnv`, `allowSkills`, the skill labels, and the redaction limits.
- `docs/08-overlays.md`: which overlay kinds a lease may request.
- `docs/09-api-and-remote-mode.md`: `workflow_prerequisites`.

## What infact does until this lands
infact 0021:
- commits the project's commit identity as `.awman/config.json` in embedded repositories;
- refuses project overlays;
- uses only its inline planning protocol.

infact 0029 gives each worker one long-lived gateway token. infact 0030 part B removes each of these as its feature lands.
