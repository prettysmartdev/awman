# Work Item: Feature

Title: Optional local builder matched to the builtin runtime's image source
Issue: n/a

Status: Open
Precedes: [WI 0126](0126-builtin-runtime-architecture-conformance.md) — that
refactor folds this builder into its single engine-level image acquirer
(Step C1). Build on the current import paths; do not wait for or pre-empt
WI 0126's restructuring.

## Summary:

- Add the boolean `builtin.builder.enabled`, defaulting to `false`.
  With `runtime: "builtin-experimental"`, enabling it lets `awman ready`
  build Dockerfiles through the local container system selected by the
  effective image source, then import the resulting image into the builtin
  runtime's OCI cache and installed image store.
- Local `docker-local` selects Docker; `apple-local` selects Apple Containers.
  Retain `docker-store` and `apple-store` as accepted input aliases. The builder is derived from
  the source, never independently selected or silently substituted.
- For an incompatible source, emit a warning whenever an image build is
  attempted, skip the build, and continue the existing import behavior from
  that source. Enabling the builder must not invalidate an otherwise valid
  registry or archive source.
- This item specifies future behavior; it does not change the currently
  released import-only behavior until implemented. It does not expand the
  builtin runtime's supported host platforms or supersede WI 0124's gates.

## User Stories

### User Story 1:
As a: user of the builtin runtime with a local Docker or Apple image store

I want to:
Enable a builder and use the normal `awman ready` Dockerfile build workflow.

So I can:
Build and prepare images in one command while continuing to run agents in
the builtin runtime.

### User Story 2:
As a: user importing images from an archive or registry

I want to:
Receive a clear warning when requesting a build with an incompatible source.

So I can:
Keep importing from my configured source without unexpected host builds or
an implicit switch to another image store.

## Implementation Details:

### Configuration and source selection

Example using Docker:

```json
{
  "runtime": "builtin-experimental",
  "builtin": {
    "builder": { "enabled": true },
    "imageSource": { "type": "docker-local" }
  }
}
```

For Apple Containers, use `"imageSource": { "type": "apple-local" }`.

- Make `docker-local` the canonical Docker image-source type, matching
  `apple-local`. Continue accepting `docker-store` as a deserialization alias,
  just as `apple-store` remains an alias for `apple-local`. This naming change
  does not select an image source implicitly or change endpoint behavior.
- Always serialize and display the canonical `docker-local` and `apple-local`
  values, including source-kind identities, configuration output, error
  messages, warnings, hints, help, examples, and user-facing documentation.
  Keep legacy names only in compatibility implementation/tests and internal
  migration notes; do not present them as recommended or alternative values
  in public documentation.
- Normalize aliases before source resolution and cache-key construction so
  either spelling selects the same source and builder. Preserve readability
  of existing configuration, image identities, and cache records containing
  legacy values; account for existing source-kind-based cache keys so the
  rename does not strand valid entries or require an unnecessary reacquisition.
- Persist `enabled` inside the `builder` object inside `builtin`; do not add
  a top-level `builder` or dotted JSON key. Retain the existing camelCase
  `imageSource` spelling. Accept only boolean `true` and `false`; reject
  strings and other types with a `builtin.builder.enabled` configuration error.
- Support global and repository config, with repository values overriding
  global values. An omitted builder or omitted `enabled` field inherits the
  global value or defaults to `false`; explicit repository `false` overrides
  global `true`.
- Add `builtin.builder.enabled` to configuration metadata, validation, `config show`,
  and `config set` for both scopes. The setting has no effect on other
  selected execution runtimes.
- Resolve each agent image using existing precedence: `builtin.images[tag]`
  before `builtin.imageSource`. Determine its builder from that resolved
  source, including the actual endpoint and source reference.

| Effective source | Builder when enabled | Import behavior |
|---|---|---|
| Local `docker-local` | Local Docker Engine through the existing Docker build integration | Export from that same Engine, validate, cache, and install |
| `apple-local` | Local Apple Containers through the existing Apple build integration | Export through the existing Apple image-source bridge, validate, cache, and install |
| `archive` | Warn and skip each attempted build | Existing archive import |
| `registry` | Warn and skip each attempted build | Existing registry acquisition |
| Remote Docker endpoint | Warn and skip each attempted build | Existing Docker-source acquisition, where supported |
| No configured source | No inferred builder | Existing source-required/reuse behavior and guidance |

Local Docker eligibility must be based on the resolved endpoint, not merely
the source type or a reachable daemon. In this item, only a local Unix socket
on a Unix host is eligible; named pipes, TCP and other transports are not,
including loopback TCP. (Amended 2026-10-01 to match the shipped code: the
builtin runtime does not support Windows hosts, so a named pipe never applies.) Do not let an ambient Docker context or
`DOCKER_HOST` redirect a build away from the endpoint used for acquisition.
Do not replace an explicit source endpoint with the default Docker store.

### Ready build and import lifecycle

1. Keep the selected execution runtime `builtin-experimental`. Build work is
   delegated to the matching host container system; no agent execution is
   moved to that system or directly onto the host.
2. Reuse the matching runtime's existing Dockerfile preparation, build
   planning, project base-image dependency ordering, repository build context,
   image tags, build arguments, platform selection, output streaming, and
   cancellation behavior. Do not invent a second Dockerfile build pipeline.
3. Plain `ready` builds when the ordinary matching-runtime readiness rules
   require it, including missing build images. `ready --build` forces the
   usual rebuilds and covers the usual non-default agent Dockerfiles.
   Forward `--no-cache` to builds whenever those rules schedule a build;
   preserve the matching runtime's flag semantics rather than treating the
   flag as a new independent build trigger. Preserve audit-triggered rebuilds
   where the normal ready flow supports them.
4. Build the project base image before dependent agent images, in the same
   local store as each dependent build. The base image is a build input;
   there is no need to install it into the builtin runtime merely to run an
   agent. With agents using different local stores, prepare the base image
   in each required store and deduplicate shared dependencies per store.
5. Build or tag the agent output at the effective source reference, then
   import it under awman's expected agent tag. Honor explicit references;
   do not build one tag and accidentally import an older, differently named
   image. Reject unwritable digest references and conflicting output targets
   before invoking a build. Retain the existing guard against one default
   fixed-reference source being reused for unrelated agent images; per-image
   mappings must distinguish those outputs.
6. After every successful agent-image build, force fresh acquisition from
   that exact store and reference. Bypass the old acquisition-cache hit even
   for plain `ready`, and update the builtin installed image and identity.
   Preserve archive validation, platform checks, publication ordering, leases,
   and in-use-image protections. A successful build alone is not readiness.
7. If no build is needed, continue the ordinary import/reuse path. If an
   image is already in the local source store but absent from builtin storage,
   import it without an unnecessary rebuild, subject to explicit build flags.
8. Report build and import as separate progress/results through CLI, TUI,
   and API. Complete readiness only after all required stages succeed.

With the builder disabled or absent, retain the existing import-only
behavior, including `--build` / `--no-cache` acquisition refresh semantics.

### Incompatible sources and failures

- Warn when the readiness plan attempts a build, including explicit rebuilds
  and implicit missing-image or audit-triggered builds. Do not warn merely
  because configuration is loaded or when a command only launches, inspects,
  or imports an already available image without attempting a build.
- Emit a warning for each affected image/build request, naming the image,
  source kind, skipped build, and continued import. Do not repeat it on every
  progress redraw or acquisition retry. Example:
  `builtin runtime builder is enabled, but image '<tag>' uses source
  'archive', which cannot build Dockerfiles; skipping build and importing
  from the configured source.`
- For incompatible sources, invoke no builder, perform no builder dependency
  installation or probing, and continue normal source acquisition. Requested
  refresh flags still apply. A successful import with this warning is not a
  build failure; an import failure retains the normal failure result.
- A compatible but unavailable builder is a build failure, not an incompatible
  source. Missing CLI, unavailable daemon, unsupported Apple host, build
  failure, and cancellation must not silently import an old cached image and
  claim success. Report the actionable cause using the existing ready failure
  and per-agent result conventions.
- A build that succeeds followed by an import/install failure is reported as
  build success and readiness failure. Preserve a previously installed image
  where existing transactional behavior permits; do not describe it as the
  newly built image. A later ready run must be able to retry the import.
- Keep warnings and errors on the existing typed message/result paths so
  machine-readable output remains valid. Use `builtin runtime` and
  `builtin-experimental` in public wording; Docker and Apple Containers may
  be named as the selected builder.

## Edge Case Considerations:

- Mixed per-image sources: build compatible agents and warn/import incompatible
  agents independently. Preserve existing non-default-agent scope and failure
  aggregation; one source must not redirect another agent's build.
- Serialize or otherwise protect build/export operations that target the same
  store/reference within awman. Capture/verify the built image identity during
  acquisition so a concurrent retag cannot silently import different content.
- Build for the Linux guest's required architecture, including on macOS;
  fail clearly on unsupported output platforms rather than installing an
  unusable image. Do not add a new platform support promise.
- Preserve runtime ownership and image-use checks during replacement. Do not
  stop running builtin sessions merely to replace their image.
- Reuse existing cancellation, timeout, disk-space, temporary-file cleanup,
  and secret-redaction conventions. An interrupted operation must not publish
  partial success or leave a new cache reference pointing to incomplete data.
- External container-system build caches remain owned by those systems.
  This feature does not add global pruning, change `awman clean`, or introduce
  cache eviction as a side effect of building/importing.

## Test Considerations:

- Configuration tests: default, both boolean values, invalid types, nested
  JSON shape, omitted builder versus omitted enabled field, global
  and repository inheritance, explicit disable, config CLI roundtrip, and
  no effect on non-builtin execution runtimes.
- Source-resolution tests: Docker local/remote endpoints, both Docker and
  Apple canonical and alias spellings, archive, registry, absent source, per-image overrides,
  explicit references, fixed-reference conflicts, and ambient endpoint drift.
- Verify both legacy aliases deserialize but serialize/display as their
  canonical local names; cover old configuration, persisted identities and
  cache references, equivalent cache lookup across spellings, and public
  errors/hints/help containing only canonical source names.
- Ready tests with recording builders/importers: base-before-agent ordering,
  ordinary missing-image behavior, forced builds, no-cache forwarding,
  audit rebuilds, multi-agent/multi-store behavior, and unchanged disabled mode.
- Seed stale source-reference and installed-image state, perform a build that
  changes content, and assert forced acquisition and installed identity match
  the new output even when no explicit refresh flag was supplied.
- Verify incompatible build attempts emit the warning, make zero builder
  calls, and still acquire from the original source. Cover successful and
  failed imports and absence of warnings on non-build paths.
- Exercise builder unavailability/failure/cancellation, import failure after
  build success, in-use images, concurrent retagging, and retry recovery;
  assert no false readiness or stale-image success.
- Verify CLI/TUI/API expose distinct build/import results and compatible
  warning semantics without corrupting structured output.
- Add opt-in real local Docker and Apple Containers scenarios on supported
  hosts: build disposable project/agent images, import, boot with the builtin
  runtime, verify a changed marker after rebuilding, and verify cached builtin
  execution no longer needs the builder service. Use existing test isolation
  gates and report missing native prerequisites as skips, not passing evidence.

## Codebase Integration:

- Follow the four-layer architecture: config types and typed result data in
  `src/data/`, build/import orchestration in `src/engine/`, effective settings
  passed by `src/command/`, and rendering only in `src/frontend/`.
- Extend `src/data/config/builtin_runtime.rs`, config field metadata and
  effective-config merging; keep source selection centralized in
  `src/engine/oci/resolve.rs`.
- Update `ImageSourceSpec` and `ImageSourceKind` in
  `src/data/config/image_source.rs`, their serde aliases and display values,
  acquisition-cache compatibility, and every public source-name formatter.
- Integrate the plan into `src/engine/ready/mod.rs` and its phase/result types.
  Reuse build behavior in `src/engine/container/docker.rs`, `apple.rs`, and
  the shared host CLI backend. Preserve builtin acquisition/install handling
  in `src/engine/container/builtin/backend.rs` and `src/engine/oci/`.
- Preserve Apple source acquisition through its existing bridge; allowing
  the Apple CLI for opted-in builds does not replace the import bridge or
  authorize unrelated host helpers.
- Update other shared image-preparation entry points as necessary to honor
  the same policy and avoid inconsistent warnings or accidental direct host
  builds. Do not silently broaden which commands trigger automatic builds.
- Upon implementation, update the current import-only statements in
  `aspec/uxui/cli.md` and relevant architecture/security specifications to
  describe this explicit, source-matched build exception.

## Documentation

After implementation, update existing user guides rather than publishing a
work-item-specific guide:

- `docs/07-configuration.md`: builder values, default, scope/precedence, and
  Docker/Apple examples.
- Across user-facing docs, configuration examples, CLI help, errors, and
  hints, document Docker sources as `docker-local` and Apple sources as
  `apple-local`; remove public examples and alias notes using legacy names.
- `docs/11-runtimes.md`: eligibility, normal ready build/import behavior,
  refresh/no-cache distinctions, warning fallback, required local tooling,
  and troubleshooting for failed builds versus failed imports.
- `docs/00-getting-started.md` and relevant ready help: optional one-command
  local build/import setup; retain the import-only path for users who do not
  enable a builder.
- `docs/04-security-and-isolation.md`: explicitly enabled local container
  builds, repository build-context boundaries, unchanged builtin execution,
  and the distinction between build helpers and VM runtime helpers.
- Keep dependency attribution confined to the existing quiet note at the
  bottom of the runtimes documentation.
