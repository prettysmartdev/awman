# Work Item: Feature

Title: Content-keyed images: rebuild stale project and agent images, reuse them across leases, clean them up, and allow `ready` over the API
Issue: issuelink

Depends on: 0143 (lease clones and lease provisioning), 0148 (`init` over the API, the model for an API-allowed setup command), 0158 (`ready` source and error handling). Slots after 0162 in awman's order.

## Summary:
- Image tags come from the repository's folder name (`src/data/image_tags.rs`: `awman-<stem>:latest`, `awman-<stem>-<agent>:latest`). Without `force_build`, an installed tag is reused (`src/engine/image_prep/build.rs`), and nothing compares it with the `Dockerfile.dev` or `.awman/Dockerfile.<agent>` it was built from. Two problems follow, one for each kind of session.
- **Local and push sessions run stale images.** An agent often adds a tool to `Dockerfile.dev` so that later steps (teardown's build, tests, lint) can use it. After that change merges, every later session on the repository still runs the old image, so teardown fails with `command not found` for a tool the repository now asks for. New projects hit this most, because their first runs are the ones that set up the toolchain. The only fix is `awman ready --build`, and `READY` is not API-allowed (`src/command/dispatch/catalogue/core.rs`), so a push client has to run the CLI on the daemon's host.
- **Leases never reuse an image, and never remove one.** A lease clone lives at `<workRoot>/leases/<lease_id>`, so its stem is the lease id. Every lease builds the project image and each agent image from scratch under a new tag. It is never stale, but every run pays a full build, and the lease path removes the clone but leaves its images behind. A busy worker fills its image store until `awman clean` is run by hand.
- Fix both with one rule: **an image is identified by what it was built from.**
  - Label every built image with a hash of its inputs.
  - Reuse an image only when the hash matches, and rebuild when it differs.
  - Leases look up images by repository and hash instead of by lease id.
  - The worker removes lease images it no longer needs, within a configured budget.
  - `ready` becomes API-allowed, so a push client can force a rebuild.

## User Stories

### User Story 1:
As a: client running workflows in push sessions

I want to:
have each session run on images built from the `Dockerfile.dev` and `.awman/Dockerfile.<agent>` in the commit it checked out

So I can:
rely on teardown having the tools that an earlier, merged change added

### User Story 2:
As a: work server operator

I want to:
have the second work item for a repository start on the images the first one built, when the Dockerfiles have not changed

So I can:
keep each node's first-step latency to seconds instead of a full image build

### User Story 3:
As a: worker owner

I want to:
have the worker remove lease images beyond a budget I set

So I can:
leave a worker running for weeks without its disk filling

### User Story 4:
As a: push API client

I want to:
`POST /v1/worker/commands {"subcommand":"ready","args":["--build"]}` in a session

So I can:
force a rebuild without the CLI or a filesystem shared with the daemon

## Implementation Details:

### Input hashes
- Layer 0 computes the hash of an image's inputs with a pure function:
  - **project image**: the bytes of `Dockerfile.dev` plus the files its `COPY`/`ADD` instructions name inside the build context, where those can be resolved without running a build. Otherwise only the Dockerfile; document that.
  - **agent image**: the bytes of `.awman/Dockerfile.<agent>` (including one that lease provisioning generated) plus the project image's id.
- Every image awman builds gets the label `dev.awman.inputs=<sha256>`, plus `dev.awman.repo=<repo key>`. The repo key is the origin URL (sanitised, with no credentials) for a remote, or `repo_hash(git_root)` for a local repository. For imported images (`runtime.builtin.imageSource` registry or archive), the hash is the source digest instead.

### Local and push sessions: rebuild when stale
- In session setup and workflow preflight, compare the installed tag's label with the hash of the session's working tree. On a mismatch, rebuild under the existing build leases, project image before agent images, and report progress on the command's event stream as `ready` does. An image without the label counts as a mismatch once, so existing installs converge.
- A rebuild that fails keeps the previous image and posts a structured `image_stale` warning event naming the failed build. A client must be able to run the session that repairs a broken `Dockerfile.dev`. `ready --build` still fails hard.
- Branches with different Dockerfiles that keep alternating rebuild on every switch. The lease rule below avoids this for leases. For named tags it is accepted and documented.

### Leases: content-keyed reuse
- Lease provisioning (0143, `src/command/worker/provision.rs`) tags images as `awman-<repo key>-<inputs hash>[-<agent>]`, not by the lease clone's folder. Before building, it checks the store for that tag with a matching label. When it is present, the lease uses it with no build. Agent images key on the project image's id, so a changed `Dockerfile.dev` gives new agent images too.
- Concurrent leases of one repository and hash share one build through the existing build leases. The second waits, then finds the tag.
- Advertise the pull capability `lease_image_reuse`.

### Cleanup
- Record each lease-built image with its last-used time in the worker's private state. After a lease ends, and at worker start, the worker removes lease-built images that no active lease uses, oldest first, until it is within `workServers.<name>.imageBudget` (`{maxImages?, maxBytes?}`, default `{maxImages: 20}`). It never removes an image it did not build for a lease, or one an active lease uses.
- Removal failures are logged, never fail a lease. `awman clean` keeps its current behavior and also lists lease images.

### `ready` over the API
- Mark `READY` `api_allowed: true` on the worker feature's push routes, with Layer 2 picking headless defaults as 0148 does for `init`:
  - `--refresh` is refused, because the audit runs an agent outside a command;
  - 0158's source prompt is never offered; a missing source is the non-interactive error.
- Its builds report through the command's event stream.
- Advertise `api_ready` and `image_inputs` (setup rebuilds stale images) in `capabilities`.

## Edge Case Considerations:
- Two push sessions of one repository set up at once with a changed `Dockerfile.dev`: the build leases serialise them, and the second sees the new label and skips.
- A `Dockerfile.dev` that fetches unpinned content (`curl … | sh`, `apt-get` without versions) gives the same hash for different results. Content keying cannot see that. `ready --build` and `ready --build --no-cache` remain the escape hatches. Document it.
- A lease whose clone has no `Dockerfile.dev` uses awman's default base. Its hash is the default Dockerfile's bytes, so all such repositories share one project image.
- Squad task workspaces keep their `squad-<slug>` stem (`image_stem`) for named tags; the label check applies to them the same way.
- Budget smaller than one lease's images: keep the active lease's images and warn once.

## Test Considerations:
- Pure tests: hash stability across file order and mtimes; a change to `Dockerfile.dev`, a copied file or an agent Dockerfile changes the right hashes; a sanitised repo key carries no credentials.
- Session setup: a stale label rebuilds once and a second session does not; a missing label rebuilds once; a failed rebuild keeps the old image and posts `image_stale`.
- Leases, against the mock server: two items of one repository with unchanged Dockerfiles build once; a changed `Dockerfile.dev` builds new project and agent images; two concurrent leases share one build; cleanup respects the budget and never removes an image in use.
- API: `ready --build` is accepted, streams progress and ends `done` or `error`; `--refresh` is refused; the capability flags are present only when their paths work.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`:
  - Layer 0 (`src/data/image_tags.rs` and a new pure inputs module): the hash, the repo key and the tag forms.
  - Layer 1 (`src/engine/image_prep/`, `src/engine/container/runtime.rs`): labels, lookup by label, removal.
  - Layer 2 (`src/command/commands/ready.rs`, `src/command/worker/provision.rs`, `policy.rs`): when to rebuild, the warn-and-continue rule, the lease budget, and API headless defaults.
  - Layer 3: maps the `ready` request and result.

  Image prep stays the only builder; leases and sessions call the same functions.

## Documentation
- `docs/11-runtimes.md`: when images are rebuilt, the labels, and the unpinned-content caveat.
- `docs/17-workers-and-work-servers.md`: lease image reuse and `imageBudget`.
- `docs/14-command-reference.md`: `ready` is API-allowed.
- `docs/09-api-and-remote-mode.md`: `image_inputs`, `api_ready`, and the `image_stale` event.

## What infact does until this lands
Nothing in code: in worker mode, every lease builds fresh images, so they are never stale. infact 0026 measures first-run latency per node, which this item reduces.
