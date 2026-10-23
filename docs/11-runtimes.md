# Runtimes

awman supports four agent runtimes. The runtime controls how agent processes are isolated from your host machine. All four use the same `awman` commands, the same workflow files, and the same agent names — the runtime is a configuration choice, not a different tool.

---

## Choosing a runtime

| Config value | Platform | Isolation unit | Requires |
|---|---|---|---|
| `docker` (default) | Linux, macOS, Windows | Linux container (shared kernel) | Docker daemon |
| `apple-containers` | macOS 26+ only | Lightweight VM per container | macOS 26 Tahoe |
| `docker-sbx-experimental` | macOS arm64; Windows x86_64 | MicroVM per session | `sbx` CLI + Docker account |
| `builtin-experimental` | macOS on Apple Silicon only (experimental) | MicroVM per session, embedded in awman | The `awman-macos-arm64` release binary (or a signed source build). No Docker, `container`, or `sbx` needed to *run* agents |

Set the runtime in your global config:

```sh
awman config set --global runtime docker                   # default
awman config set --global runtime apple-containers         # macOS 26+ only
awman config set --global runtime docker-sbx-experimental  # experimental
awman config set --global runtime builtin-experimental     # experimental, Apple Silicon only; see below
```

You can switch runtimes at any time — each keeps its own separate state and does not interfere with the others. See [Switching runtimes](#switching-runtimes).

**Invalid values are a fatal error.** If the `runtime` field holds anything other than the four strings above, awman refuses to start — it does not fall back to Docker, because launching agents under a different isolation model than the one you configured would be unsafe. CLI commands print the invalid value and the valid options, then exit immediately; the interactive TUI opens a modal dialog on startup with the same message (press Enter to quit). Fix the `runtime` value in `$HOME/.awman/config.json` and relaunch.

---

## Docker (default)

Docker is the default runtime on all platforms.

**Requirements:** Docker daemon running.

**Isolation model:** standard Linux containers. The agent runs in a container with your project mounted read-write. The container shares the host kernel; host escape requires a container escape exploit.

**Lifecycle:** containers are ephemeral. The agent container is removed when the session ends. Persistent state lives in your Git repository.

**`awman ready` for Docker** builds per-agent Docker images from the `Dockerfile.<agent>` files in `.awman/`. Images are rebuilt when you run `awman ready --build` or when another normal ready rule schedules a build; `--no-cache` disables build cache for builds that run.

See [Security & Isolation](04-security-and-isolation.md) for overlay mounts, the Docker socket flag, and worktree isolation.

---

## Apple Containers

Apple Containers is a macOS-native runtime that runs each agent in a lightweight virtual machine. It uses the native `container` CLI rather than Docker.

**Requirements:** macOS 26 Tahoe or later. The `container` CLI comes with macOS 26 — no separate install.

**Isolation model:** each container is a lightweight Linux VM using Apple's native hypervisor. Host escape requires a hypervisor exploit.

**Limitations:**
- macOS only — not available on Linux or Windows. Configuring it on those platforms is an error, not a silent fallback.
- `--allow-docker` (host Docker socket mount) is not supported under this runtime.
- Agent attach is available only while the awman process that launched the agent is still running. Once that process exits, the agent terminal cannot be reattached.

**`awman ready` for Apple Containers** builds the same per-agent images used by Docker, using `container build` instead of `docker build`. Images are rebuilt when you run `awman ready --build` or when another normal ready rule schedules a build; `--no-cache` disables build cache for builds that run.

---

## Builtin microVM (experimental)

> **Current source build availability:** the runtime dependency upgrade is awaiting native Apple Silicon verification. `make payloads` on Apple Silicon currently records extraction evidence and exits nonzero because the new Darwin payload is unverified, so `make build` and `make install` build without the builtin runtime. To verify it yourself on an Apple Silicon Mac, commit or stash local changes, install Apple Containers (Docker is optional), and run `make payloads-verify` as your own user (not with `sudo`). It boots real guest VMs, runs the required hardware and signed-artifact checks, and marks the payload verified in `third_party/msb-payloads/manifest.toml` only if all of them pass. After that, `make install` includes the builtin runtime. Use another runtime or a previously verified release until the new payload, signed/stripped artifact and guest checks pass. Publishing the next release is blocked on those checks.

`builtin-experimental` runs each agent session in a Linux microVM whose VM code, kernel, and guest agent are **embedded in the awman executable**. Each VM starts another process of that same executable. There is no separate runtime to install, and no Docker, `container` or `sbx` is needed to *run* agents.

**Isolation model:** a hardware-virtualized VM with its own kernel per session. The hypervisor, device emulation, and shared-filesystem implementations form part of the security boundary. The configured project, settings, and overlay mounts grant access to host files. There is no host Docker socket bridge.

> **Experimental, macOS on Apple Silicon only.** This runtime is released as an experimental preview, like `docker-sbx-experimental`: behaviour and configuration may still change. In this release it runs only on Macs with Apple Silicon. The previous dependency version's guest behaviour (sessions, PTY and ACP, workflows, squad, the API, resource limits, networking, image import) was tested on real Apple Silicon hardware; the current dependency upgrade has not re-earned those results; see [Parity limits](#parity-limits-versus-docker-and-apple-containers) for what differs from the other runtimes.

### Where it is available

| Host | Status |
|---|---|
| macOS on Apple Silicon | Previous release supported (experimental); current source build awaits payload and guest re-verification (see above). |
| Linux (x86_64 or arm64), Windows | Not available in this release. Selecting it reports `the experimental builtin runtime is only available on macOS on Apple Silicon (macos/aarch64) in this release; this host is <os>/<arch>`. |

The runtime is present only in awman builds compiled with it. The `awman-macos-arm64` release asset includes it; the Linux and Windows assets do not. A build without it reports `the builtin runtime is unavailable: this build of awman does not include the builtin-runtime feature`.

### Enabling it

The runtime is only ever used when you select it explicitly in your **global** config (`~/.awman/config.json`). It is never a default and a repository config cannot select it:

```sh
awman config set --global runtime builtin-experimental
awman ready
```

or edit `~/.awman/config.json` directly:

```json
{ "runtime": "builtin-experimental" }
```

The value is exact: `builtin` (without `-experimental`) is not a runtime name and is refused with a message pointing to `builtin-experimental`. `awman config set` refuses unknown runtime values, and if the file already holds one, `awman config …` still works so you can fix it; every command that would run an agent refuses to start until you do.

### The macOS binary, Gatekeeper and permissions

macOS only lets a program start a virtual machine when its code signature carries Apple's `com.apple.security.hypervisor` entitlement. The `awman-macos-arm64` binary is therefore **ad-hoc signed** with exactly that entitlement. It is **not** signed with an Apple Developer ID and **not** notarized. What that means for you:

- **Gatekeeper and quarantine.** A file downloaded with a browser, or received by AirDrop, Mail or Messages, is marked as quarantined, and macOS refuses to open it ("cannot be opened because the developer cannot be verified"). Installing with the installer (`curl -s https://prettysmart.dev/install/awman.sh | sh`), or downloading with `curl` or `gh release download`, does not add the quarantine mark. If you downloaded with a browser, first verify the file against the published checksum (below), then remove the mark:

  ```sh
  xattr -d com.apple.quarantine ./awman-macos-arm64
  ```

- **Verify the checksum.** An ad-hoc signature proves the file has not changed since it was signed, not who signed it. The only way to confirm you have the genuine release is to compare its SHA-256 with the value published on the release page:

  ```sh
  shasum -a 256 awman-macos-arm64
  ```

- **Don't modify the binary.** Anything that changes the file after signing (for example `strip`, patching, or a tool that relinks it) invalidates the signature, and macOS then refuses to run it or to start VMs from it. Re-sign it (see *Building from source*) or download it again.
- **Folder-access prompts.** macOS asks before a program reads `~/Desktop`, `~/Documents`, `~/Downloads`, iCloud Drive or removable volumes. When you run awman from a terminal, that permission belongs to your terminal app, so upgrading awman changes nothing. The **squad daemon** runs in the background under launchd, so there the permission belongs to awman itself — and because the signature is ad-hoc, macOS treats every new awman version as a new program. If a squad task works in a repository under one of those folders, macOS asks again after each awman upgrade, and the task waits until you answer. Keep squad workspaces elsewhere (for example `~/Workspaces`) to avoid this, or review the grants in **System Settings → Privacy & Security → Files and Folders**. The keychain is not affected: awman reads credentials through Apple's own `security` tool, so keychain access does not change when awman does.

### Building from source

After the native verification blocker above is resolved, run `make payloads` once (it fetches and checksum-verifies the embedded kernel and guest agent for your host), then `make install`. On Apple Silicon, `make build`/`make install` include the builtin runtime when the payload verifies and **ad-hoc sign the binary with the hypervisor entitlement automatically** (`tools/macos-sign.sh`). On other hosts they build the existing runtimes only.

If you copy, strip or otherwise change the binary afterwards, sign it again:

```sh
make sign-macos BIN=/usr/local/bin/awman
```

An unsigned build fails to start agents with `embedded worker startup failed; verify KVM access or the macOS com.apple.security.hypervisor entitlement …`.

### Setup

```sh
awman config set --global runtime builtin-experimental
awman ready
```

By default, `awman ready` **imports** a finished image from a source you choose. To opt into source-matched local builds, see [Building from a local image source](#building-from-a-local-image-source).

### Getting an agent image

Docker and Apple Containers build `awman-<project>:latest` and `awman-<project>-<agent>:latest` from `Dockerfile.dev` and `.awman/Dockerfile.<agent>`. By default the builtin runtime imports finished images, but you can opt into source-matched builds with `builtin.builder.enabled`; see [Building from a local image source](#building-from-a-local-image-source). Without that setting, build the images yourself and tell awman where to import them from.

**On a Mac, the simplest route is Apple Containers.** If you already use the `apple-containers` runtime, the images are already in its store: set `"imageSource": {"type": "apple-local"}` and run `awman ready` (see [Apple Containers store](#apple-containers-store)). Otherwise build them with `container build` (same arguments as `docker build` below) and use the same source. Docker, a registry, or an archive file work too:

**1. Build the images** from the repo root, in this order (the agent image is `FROM` the project image). Running `awman ready` first prints these exact commands for your project and agent, and downloads `.awman/Dockerfile.<agent>` if it is missing:

```sh
docker build -t awman-myproject:latest -f Dockerfile.dev .
docker build -t awman-myproject-claude:latest -f .awman/Dockerfile.claude .
```

Build for the host's architecture (`linux/amd64` on x86_64 hosts, `linux/arm64` on ARM hosts, including Apple Silicon). The build can happen on another machine.

**2. Choose an image source** in `builtin.imageSource` (or per image in `builtin.images`) in your global or repo config JSON:

```jsonc
// one of:
{ "type": "docker-local" }                                   // export from a local Docker Engine
{ "type": "apple-local" }                                    // macOS: export from the local Apple Containers image store
{ "type": "archive", "path": "/abs/path/claude.tar" }        // after: docker save awman-myproject-claude:latest -o /abs/path/claude.tar
{ "type": "registry", "registry": "ghcr.io", "reference": "acme/awman-myproject-claude:latest" }  // after pushing
```

**3. Import:**

```sh
awman ready
```

The image is imported into awman's private store. Docker is used only as a *source* at this step (awman talks to the Engine API directly and never runs the `docker` CLI). Agents never run in Docker under this runtime, and after import Docker does not need to be running.

There is no default source. With none configured, `awman ready` fails with `no image source is configured for '<tag>'` followed by the build commands above, rather than guessing a registry or daemon.

**`awman init` and `builtin-experimental`.** `awman init` builds images through the configured runtime, so with `runtime` set to `builtin-experimental` its build step fails (`image building; build externally and import with ready is not supported on the builtin runtime`) and the agent audit is skipped. Run `awman init` before selecting `builtin-experimental` (or with `docker` or `apple-containers` temporarily selected), then switch the runtime and run `awman ready` after building externally.

### Image sources

Every source is explicit. A source never falls back to another one, and the same name from a registry and from a Docker daemon are treated as different images. Each source has a `type`; unknown `type` values and unknown fields are rejected.

| `type` | Fields | Behavior |
|---|---|---|
| `registry` | `registry`, `reference` (at least one required) | Pulls the image for the host platform from an OCI registry. `reference` defaults to the awman image tag on `registry`; a `reference` naming a different registry than `registry` is refused. Docker Hub names are normalized (`library/`). Manifests and blobs are verified against their digests. |
| `docker-local` | `host`, `tls`, `reference` (all optional) | Exports the image from a Docker Engine over its API (Engine API 1.41 or newer). `host` defaults to `$DOCKER_HOST`, then the platform default socket (`/var/run/docker.sock`; on macOS `~/.docker/run/docker.sock` is tried first). `host` must start with `unix://` or `tcp://`; plain `tcp://` is allowed only for loopback addresses. `ssh://` and `npipe://` are refused as unsupported. `tls` (`ca`, optional `cert` and `key`, `verify` default `true`) applies only to `tcp://` hosts; `DOCKER_TLS_VERIFY` and `DOCKER_CERT_PATH` are honored when the host comes from `DOCKER_HOST`. |
| `archive` | `path` (absolute) | Reads a tar from disk: `docker save` output, an OCI image layout, or either one gzip-compressed. Re-imported automatically when the file changes. |
| `apple-local` | `reference` (optional) | macOS only. Exports the image from the local Apple Containers image store (see [Apple Containers store](#apple-containers-store)). |

`reference` defaults to the awman image tag (`awman-<project>-<agent>:latest`) when omitted.

#### Apple Containers store

With `{ "type": "apple-local" }`, `awman ready` asks the Apple Containers service to export the image. awman sends the request itself, from inside the `awman` process. It never runs the `container` CLI, never starts a helper program of its own, and never reads Apple's private store files.

- **Supported releases.** Apple's image-store protocol has no version number, so awman checks the service's release and commit before it asks for any image. The supported releases are 0.12.0, 1.4.1 and 1.5.0. Any other release, including a newer one or a local build, fails with `apple-local image source is not supported: Apple Containers <version> … is not a release awman has verified`. Check your release with `container system version`.
- **The service must be running.** If it isn't, `awman ready` fails with `the Apple Containers service is not running` and tells you to run `container system start`.
- **Exactly one image.** The name must match exactly one image in the store. `docker.io/library/` and a missing `:latest` tag are normalized. If no image matches, or two different images match, the import fails instead of guessing.
- **Same checks as every other source.** The export is written into awman's private staging area, then checked for platform (`linux/arm64`), digests, size limits and format before it is cached. A cancelled or failed export leaves nothing behind, and an image that was already imported stays usable.
- **Works offline afterwards.** Once imported, the image runs from awman's cache. The Apple Containers service can be stopped and does not need to be running.

**Per-image overrides.** `builtin.images` maps an awman image tag to its own source; it wins over `builtin.imageSource` for that tag:

```json
{
  "builtin": {
    "imageSource": { "type": "docker-local" },
    "images": {
      "awman-myproject-claude:latest": { "type": "archive", "path": "/opt/images/claude.tar" }
    }
  }
}
```

**Registry settings.** `builtin.registries` is keyed by `host[:port]`:

```json
{
  "builtin": {
    "registries": {
      "registry.example.com": {
        "caCert": "/etc/ssl/corp-ca.pem",
        "auth": { "type": "env", "usernameVar": "REG_USER", "passwordVar": "REG_PASS" }
      },
      "localhost:5000": { "insecure": true }
    }
  }
}
```

| Key | Meaning |
|---|---|
| `insecure` | `true` allows plain HTTP for this host only (default `false`) |
| `caCert` | PEM bundle of extra trusted CAs for this host |
| `auth` | Where credentials come from; secrets are never stored in config. `{"type":"anonymous"}`; `{"type":"env","usernameVar":…,"passwordVar":…}` reads named environment variables; `{"type":"keychain","service":…}` reads an item from awman's OS keychain (account = registry host, value `username:password`); `{"type":"docker-config"}` reads inline `auths` entries from `~/.docker/config.json` |

`docker-config` supports inline credentials only. If the file names a `credsStore` or `credHelpers` entry, awman reports an authentication error naming the helper and does **not** execute it; use `env` or `keychain` instead. Credentials are sent only to the registry itself (and Docker Hub's own token service); a token server on any other host is contacted anonymously. Registry pulls honor `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY`.

### What `awman ready` does under `builtin-experimental`

- With the builder disabled, the base project image is reported as **skipped** (it is only an input to your external build). With the builder enabled and a compatible local source, it is built in that source store first (see [Building from a local image source](#building-from-a-local-image-source)). The agent image is **imported**, and an **Image source** row appears in the summary (also in `--json` output).
- One rule decides whether an image is acquired, for every command that prepares images: it is re-acquired when the configured source differs from the one the installed image came from, or when a successful build is waiting to be imported; otherwise the installed or cached copy is reused. `awman ready`, API session setup, and `chat`/`exec prompt` launches may also build (when `builtin.builder.enabled` is true and the image's source is a compatible local one); session setup and launches build only when the image is missing; workflow preflight and squad are import-only and never start a build. The `Dockerfile` path used for builds and hints is the repo's configured one.
- With the builtin builder enabled, a chat or prompt launch whose agent image is not installed prepares it first, as Docker and Apple Containers do: it builds the base and agent images in the source store if that store lacks them (fetching `.awman/Dockerfile.<agent>` if needed), then imports the result. So `awman chat --agent codex` works without a separate `ready` run. With an incompatible source (a registry, an archive, a remote Docker endpoint) the launch imports from it without building. With the builder disabled, a launch never builds or imports: run `awman ready` first.
- With the builtin builder enabled, chat and prompt launches check an installed image's recorded source and any pending build receipt without contacting a source or builder. If the source differs or the installed content does not match the pending build, run `awman ready` before launching. An older installation without recorded source details also needs one `ready` run to establish them.
- With a configured source, `ready` checks the acquisition cache using the source kind, resolved endpoint or archive path, reference, and platform. Changing any of these selects that source's cache entry or acquires it. A replaced archive is revalidated; a cached archive remains usable after its source is removed. With no source configured, an installed image is reused.
- With the builder disabled (the default), `awman ready --build` and `--no-cache` **re-acquire** the image from its source; they do not build. `--refresh` (the Dockerfile audit) is unchanged. With the builder enabled, `--build` schedules the usual builds, and `--no-cache` applies to any build that runs and still re-acquires the image from its source.
- `awman ready --build` also imports the image of **every other agent** that has a `.awman/Dockerfile.<agent>` (one **Import agent image: \<agent\>** row each), so `chat --agent`, workflow steps, and other agents can run without a separate import. Plain `awman ready` imports only the default agent and lists any other agents whose images are missing.
- Other agents use their own `builtin.images` entry when there is one, otherwise `builtin.imageSource`. A `builtin.imageSource` that names a single image — an `archive`, or any source with an explicit `reference` — holds only one agent's image, so for other agents `ready` skips it with a warning instead of importing the wrong image. Give each of those agents its own `builtin.images` entry (for example one archive per agent). Workflow preflight and squad apply the same rule: a step running another agent fails with that warning's text rather than importing the default agent's image under its tag.
- When the summary reports agent images that are still missing, its closing hint says what to do for each one: `run "ready --build" to import <agents>.` (or `build <agents>` / `build and import <agents>`, depending on what each image's source allows), and ``configure an image source for <agents> (`builtin.imageSource` or a `builtin.images` entry), then run "ready --build".`` for an agent with no usable source. A digest-pinned local reference or colliding build output instead asks you to configure a writable, distinct reference in `builtin.images` before running `ready --build`. A failed image gets its own hint, for example `fix the <agent> image build, then run "awman ready --build" again.` or `the <agent> image was built but not imported; run "awman ready" again to retry the import.`
- If the `--refresh` audit changes `Dockerfile.dev` and this session cannot rebuild the default image, `ready` warns: ``Dockerfile changed: rebuild the image in its source store (or enable `builtin.builder.enabled` with a `docker-local` or `apple-local` source), then run `awman ready --build` to re-import it``, or, with no source configured, to rebuild it externally and configure `builtin.imageSource`.
- Older acquisition-cache references without per-source metadata must be acquired again once. Installed images remain runnable without their source.
- A transient failure while importing (a dropped connection, a stalled transfer, a 5xx from the registry or daemon) is retried automatically, up to a bounded number of attempts and an overall time limit. Errors that won't be fixed by retrying — bad credentials, a digest or platform mismatch, a rejected archive, an unsupported or blocked source — are reported immediately instead. Ctrl-C cancels acquisition and waits for cleanup. If the final runtime import has already begun, cancellation waits for that commit and may leave a complete cached image. Local filesystem or keychain calls may delay the response.
- Workflow setup/teardown steps use the imported agent image of the repo's configured agent (default `claude`) unless the workflow or config sets an explicit `baseImage`, which must already be imported. Workflow preflight imports images but never starts a host build, and it re-acquires an image that is installed from a different source than the one now configured. Prepare images with `awman ready` before starting the workflow.

### Building from a local image source

Set `builtin.builder.enabled` to `true` to let `awman ready`, and a launch whose agent image is missing, build and then import images while keeping `builtin-experimental` as the execution runtime. The setting defaults to `false` and has no effect when another runtime is selected. Each image's resolved source determines the builder: `docker-local` can use the matching local Unix-socket Docker Engine, and `apple-local` can use Apple Containers. TCP and other remote Docker endpoints, registries, and archives are not build targets. For an incompatible source, a requested build is skipped with a warning and the configured source remains the import source; awman never switches stores implicitly.

Plain `awman ready` inspects a compatible local source store when normal readiness rules need to know whether an image exists. If no build is needed, an installed image that matches that source is reused without exporting it again. If the builder cannot be reached but that image is already installed and no build is pending, ready warns and continues with the installed image. A successful build forces a fresh import from the same store and reference, even if the acquisition cache contains an older image.

The project base image is built before dependent agent images in the same source store. Build and import have separate result rows; readiness requires both stages for each required image. A project image built only for a non-default agent's store is optional like that agent's image: if it fails, the row shows the failure, but `ready` still succeeds once every required image is ready. A build can succeed while import fails, in which case `awman ready` reports failure and can retry the import on a later run. `--no-cache` affects builds when a build is scheduled; by itself it does not schedule one, but it does re-acquire the image from its source. Use `--build` to request the usual forced builds.

If a required image stage fails or remains unfinished, `awman ready` reports `awman is not ready.` and exits with status 1, whether or not the builder is enabled. Pending build receipts are stored under the awman data home at `pending-builds/`; per-image build locks are under `locks/image-build/`. Two awman processes preparing the same store and reference take turns; the one that waits (a `chat` launch during a running `awman ready --build`, for example) prints `Waiting for another awman process to finish with <reference> in <store>…`. Source builds disable BuildKit provenance and SBOM attestations so the exported result has a verifiable single-platform image. Docker `npipe://` endpoints are unsupported for builtin image acquisition or building. A configured source is checked again when an image must be acquired; a cached builtin image is reused only when its identity matches the inspected source.

Docker builds require a local Docker CLI and daemon. If Docker Buildx is installed, its `default` builder must use the `docker` driver; other drivers fail with an actionable error. Apple builds require the Apple Containers CLI/service. The source store is used for building and importing, while agent sessions continue to run only in the builtin microVM.

Importing enforces safety limits: a total archive of up to 32 GiB, up to 8 GiB per layer after decompression, up to 256 layers, and at least 1 GiB of free disk space. Archive metadata and the selected image's layers are validated before import: digests must match, the image must be for the host's Linux platform, and selected images with path traversal, writes through symlinks, device nodes, unsafe hard links, or malformed whiteouts are rejected (`image archive … rejected: <reason>`). Only the selected image and requested tag reach the runtime importer; unselected images and unrelated tags are excluded, while selected config and layer bytes are preserved. Plain and gzip-wrapped tars are supported; zstd-, xz-, or bzip2-wrapped archives, zstd-compressed layers, and non-distributable/foreign layers are refused with a reason naming the problem and, for zstd, the gzip re-export that works instead. Launching a session uses the already-imported image and never re-reads its source; only a launch with the builder enabled and its agent image missing acquires one first, as described above.

### Configuration

The `runtime` key is global; the `builtin` block can be set in the global config and per repo. Repo values override global ones per field, and the `images` and `registries` maps merge per key.

| Key | Default | Meaning |
|---|---|---|
| `builtin.stateDir` | `~/.awman/builtin` (under the awman data home) | Private builtin runtime state root for imported images, VM/session state, catalog and control sockets. The directory is process-wide: every builtin session in the awman process uses it, and a repo `builtin.stateDir` that resolves to a different directory is rejected at launch with an error naming both paths. Must be an absolute path without `..`, owned by you with mode `0700`, and contain no symlinks. Keep it short: control sockets live below it and paths are limited to about 100 bytes (`socket path … is longer than the … limit; configure a shorter builtin state directory`). `AWMAN_BUILTIN_STATE_DIR` overrides config for the process. |
| `builtin.vcpus` | `2` | Whole vCPUs per session VM, 1–255. Fractional values are rejected, never rounded. |
| `builtin.memoryMib` | `4096` | Guest memory in MiB, 128 or more. |
| `builtin.rootDiskMib` | `30720` (30 GiB) | Writable root disk per session VM in MiB, 256 or more: the space the agent has for everything outside its mounts (`/`, its home directory, installed packages). The disk file is sparse, so it only uses host space as the agent writes. |
| `builtin.mountQuotaMib` | `30720` (30 GiB) | How many MiB the agent may add to each host mount, the workspace included, beyond what it already held; 1 or more. Writes past it fail with "no space left on device" in the guest. This keeps an agent from filling your disk through a mount. |
| `builtin.imageSource` | unset | Default image source (see above). |
| `builtin.images` | `{}` | Per-image-tag sources. |
| `builtin.registries` | `{}` | Per-registry settings. |
| `builtin.network` | `{"mode":"public"}` | Guest network policy — see [Network](#network) below. |

Builtin VM settings are resolved per session from that session's own repo, in the TUI, the API and squad alike. A repo's `vcpus`, `memoryMib`, `rootDiskMib`, `mountQuotaMib`, `registries` and `network` override the corresponding global values for the sessions launched in that repo (`network` subject to the ceiling below), so two repos served by one awman process can run with different limits. `builtin.stateDir` is the one process-wide setting: a repo value that resolves to a different directory than the one the process uses refuses the launch with an error naming both paths; remove it from the repo config or set it to the process's path. `vcpus`, `memoryMib`, `rootDiskMib` and `mountQuotaMib` can be changed with `awman config set` (for example `awman config set --global builtin.vcpus 4`). The object-valued keys are shown read-only by `awman config show`; edit them in the JSON file. The `builtin` block is only used when `runtime` is `builtin-experimental`, and an invalid one is an error naming the bad key. (The settings block keeps the name `builtin`; only the runtime value carries `-experimental`.)

The builtin runtime refuses startup if any of these Microsandbox override variables is set: `MSB_PATH`, `MSB_LIBKRUNFW_PATH`, `MSB_AGENTD_PATH`, `MSB_HOME`, `MSB_BACKEND`, `MSB_CONFIG_PATH`, `MSB_PROFILE`, `MSB_CACHE_DIR`, `MSB_SANDBOXES_DIR`, `MSB_VOLUMES_DIR`, `MSB_SNAPSHOTS_DIR`, `MSB_LOGS_DIR`, or `MSB_SECRETS_DIR`. Unset the named variable and retry.

### Network

Every session VM gets its own network stack running in the host-side worker process. Hostname allow rules are limited to TCP; UDP/QUIC is denied because the stack cannot inspect its hostname boundary for those protocols. Configure it under `builtin.network`, global and/or per repo:

```json
{
  "builtin": {
    "network": {
      "mode": "allowlist",
      "allow": ["api.anthropic.com", "*.github.com"],
      "hostPorts": [8765],
      "nameservers": ["1.1.1.1", "[2606:4700::1111]:53"],
      "trustHostCas": false
    }
  }
}
```

| `mode` | Guest network device | Reachable |
|---|---|---|
| `public` (default) | yes | the public internet, plus any authorized `hostPorts` |
| `allowlist` | yes | only the names in `allow` (exact or `*.suffix`), plus any authorized `hostPorts` |
| `none` | no | nothing — a cached, already-imported image still runs offline |

In every mode: inbound connections are refused (awman never publishes a guest port); private/LAN (RFC1918/ULA/CGN), loopback, link-local and cloud-metadata (`169.254.169.254`) destinations are refused; TLS is never intercepted; and the host's own `HTTP(S)_PROXY` / `ALL_PROXY` / `NO_PROXY` are **not** applied to guest traffic — a proxy the guest is configured to use is just another destination, subject to the same policy.

**DNS and HTTPS.** The host-side stack intercepts DNS. For HTTPS authorized by a hostname rule, the visible TLS ClientHello server name (SNI) must be allowed and that exact name must have a DNS binding to the destination IP. An allowed sibling name or a shared IP alone does not authorize the connection. Missing or unlisted visible SNI is refused. The connection remains encrypted end to end: awman cannot inspect encrypted HTTP `Host`/`:authority`, domain fronting behind an allowed server, or the hidden inner name of Encrypted ClientHello (ECH). An allowed ECH outer name proves only that outer name. Use services you trust within this boundary; it is not a guarantee about every logical destination carried inside an allowed encrypted connection. A DNS answer does not authorize UDP/QUIC. Upstream resolvers default to the host's own resolver configuration, or set `nameservers` (global only; IP or `IP:PORT`, never a host name, so resolving a resolver never depends on one).

**Host-local services**, such as an MCP server listening on your host's own loopback address, are reached through `hostPorts`, not through the guest's own `127.0.0.1` — inside the VM, `127.0.0.1` / `::1` / `localhost` is always the guest's own loopback, never the host's. List the TCP port in `hostPorts`; the guest then dials `host.builtin.internal:<port>` and the host-side stack connects that to the host's own loopback on the same port. Every other host port stays refused (including port `53`, which is reserved for guest DNS).

**Layering.** The global `builtin.network` block is a ceiling a repo config cannot raise, since repo config is part of the checkout and may be untrusted: a repo may pick a stricter `mode`, a subset of the global `allow` list, and a subset of the global `hostPorts`, but never something wider — asking for more is a config error, not a silent downgrade. `nameservers` and `trustHostCas` are read from the global config only. The repo's block narrows the policy for every builtin launch in that repo, in the TUI, the API and squad.

**CAs.** By default the guest trusts only the image's own CA bundle. Set `trustHostCas: true` (global only) to also install the host's trusted root CAs in the guest at boot — useful behind a corporate TLS-inspecting proxy. awman never installs an interception CA of its own.

> The previous dependency version tested these rules in real guests on Apple Silicon; the current upgrade still requires native re-verification: allowlist and denied-egress enforcement (DNS, direct IP, DNS-over-HTTPS, proxy bypass), `hostPorts` access, `none` mode, and isolation between VMs running at the same time with different policies. IPv6 denial is verified only on hosts that have IPv6 connectivity.

### Session lifecycle

- Each agent session, and each workflow setup/teardown environment, is its own microVM, removed when the session ends.
- Ctrl-C in the launching session sends the agent an interrupt and gives it 10 seconds to exit before the VM is killed. In `awman agents attach`, Ctrl-C detaches only that client.
- **The awman process that launched a session must keep running.** If it exits, the session's VM stops. Detaching and reattaching works while that process is alive: another client, including a newer compatible awman process, can attach to the same live session and see the same output; detaching only ends that one client's connection, and the guest keeps running. Reattaching after the launching process has exited is not supported. Checkpoint restore is not supported.
- **A client is never left believing it saw the whole session when it didn't.** If a client reading attached output falls too far behind the live stream, awman ends that connection with an explicit failure — never a later clean exit that would make missed output look like it was delivered. Reconnect (or restart the agent) after seeing this. An attach starts at the current output; it does not replay what was written before the connection was accepted.
- `awman agents status` lists builtin sessions. Workflows and squad find their sessions by name, as they do on Docker.
- `awman agents detail <id>` includes the builtin VM's configured vCPUs, memory in MiB, and network mode.
- Live CPU and memory figures (the TUI's container title bar and minimized bars, and `awman agents status`) are measured from the VM itself: CPU is the guest's vCPU time over wall time on the same per-core scale as `docker stats` (100% is one fully busy vCPU, so a 2-vCPU VM tops out at 200%), and memory is what the guest is using. The runtime publishes a new sample once a second, so a one-shot `awman agents status` may take about a second per running session to report CPU.
- If you replace the awman binary while a session is running, new operations report `awman was replaced on disk while this session was running; restart awman to use the builtin runtime`. An awman with a different internal protocol version refuses to adopt sandboxes created by another version (`builtin runtime worker protocol mismatch … stop running agents started by an older awman and retry`): stop those agents with the awman that started them first. If the state directory itself was created under another protocol, the error names that directory instead, and every builtin command refuses it until it is moved aside or deleted (see [Cleaning up](13-cleaning-up.md)).

### Parity limits versus Docker and Apple Containers

| | Docker / Apple Containers | Builtin |
|---|---|---|
| Image creation | `ready` builds from Dockerfiles | `ready` imports; you build externally |
| Fractional CPU | Supported | Rejected (`cannot honour …`): whole vCPUs only |
| Resource limit enforcement | Container-level limits | VM allocation: the guest sees exactly the configured vCPUs and memory, and memory exhaustion is handled by the guest's own OOM killer without affecting the host or other VMs |
| `--allow-docker` | Mounts the host Docker socket (Docker only) | **Rejected.** No socket is ever mounted and there is no host daemon bridge |
| Reattach after the launching awman exits | Docker: yes. Apple: no | No |
| Host `localhost` / MCP services | Per the container network | Inside the VM, `127.0.0.1` is the VM itself; a host loopback service is reachable only if its port is listed in `builtin.network.hostPorts` (see [Network](#network)) |
| Egress control | None built in (Docker); per-container network (Apple) | `builtin.network.mode`: `public` (default), `allowlist`, or `none`. HTTPS checks visible SNI plus its exact DNS/IP binding; encrypted HTTP authority and ECH inner names are not inspected. Hostname rules deny UDP/QUIC |
| Tool allow/deny lists for an agent with no matching flag | See [Agent Sessions](03-agent-sessions.md) | Rejected with an error instead of being silently discarded |
| Named image `USER` (like `awman` in the templates) | Handled by the container engine | Resolved to a numeric id from the imported image's `/etc/passwd` and `/etc/group`; an unresolvable user is refused. Agents never run as root |
| Image `HOME` | From the image | From the image's `HOME`, or else the image user's home directory in `/etc/passwd` (as Docker does); an image with neither is refused |
| Exit status of an agent killed by a signal | `128 + signal` (for example `137` after the OOM killer) | `-1`: the embedded guest agent reports that the process was killed by a signal but not which one; awman's own exit status is then `1` |
| Overlays (`dir`, `skill`, `env`, `context`), agent settings and system prompts | Bind mounts / env | Same overlay semantics. Directories are directory mounts and single files are single-file mounts (the parent directory is never mounted). Sockets can't be mounted as overlays |

### Troubleshooting

| Message | Cause and fix |
|---|---|
| `the builtin runtime is unavailable: this build of awman does not include the builtin-runtime feature` | This awman build lacks the runtime. Use an asset that includes it or build from source (see above), or select another runtime. |
| `the experimental builtin runtime is only available on macOS on Apple Silicon …` | This release supports the builtin runtime only on Apple Silicon Macs. Use `docker`, `apple-containers` or `docker-sbx-experimental`. |
| `invalid runtime 'builtin' … (the builtin runtime is experimental: set runtime to "builtin-experimental" …)` | The runtime value is `builtin-experimental`. Run `awman config set --global runtime builtin-experimental`. |
| `embedded worker startup failed; verify KVM access or the macOS com.apple.security.hypervisor entitlement …` | The binary lacks the hypervisor entitlement: it was built from source without signing, or modified after signing. Run `make sign-macos BIN=$(command -v awman)`, or reinstall the release binary. |
| `"awman-macos-arm64" cannot be opened because the developer cannot be verified` | The file is quarantined (downloaded with a browser). Verify its checksum, then `xattr -d com.apple.quarantine <file>` — see [The macOS binary, Gatekeeper and permissions](#the-macos-binary-gatekeeper-and-permissions). |
| `builtin image <tag> has no absolute effective HOME` | The image sets no `HOME` and its user has no absolute home in `/etc/passwd`. Add `ENV HOME=/path` to the Dockerfile. |
| `the builtin runtime refuses to start while <variable> is set …` | An environment override would replace the builtin runtime. Unset the variable named in the error and retry. |
| `no image source is configured for '<tag>'` | Set `builtin.imageSource` or `builtin.images` (see [Getting an agent image](#getting-an-agent-image)). The message explains why, depending on whether the builder is off, on without a source, or on with a source it cannot build into. |
| ``builtin image <image> is not imported; run `awman ready` to import it``, or ``image '<tag>' is not in the builtin image cache (…); run `awman ready` to import it`` | Run `awman ready`. |
| ``a builtin sandbox for agent '<name>' already exists; stop or remove it (`awman clean`) or choose another name`` | A session with that name is still present; stop it, run `awman clean`, or pick another name. |
| `the <builder> image builder is unavailable: <detail>` | `builtin.builder.enabled` selected Docker or Apple Containers for this image's source, but it cannot be used (CLI missing, daemon or service stopped). Start it, or disable the builder to import only. This is a build failure; awman does not fall back to an older image. |
| ``the Docker builder `default` for <endpoint> uses the `<driver>` driver, which does not store images in that Engine …`` | Switch the endpoint's buildx `default` builder to the `docker` driver (`docker buildx use default`), or uninstall buildx to use the legacy builder. |
| ``<tag>: the build succeeded but the import failed; run `awman ready` again to retry the import`` | The image built, but importing it into the builtin runtime failed (see the import row for why). Fix that and run `awman ready`; it imports the build without rebuilding. |
| `<tag>: <builder> is unreachable; using the installed image (…)` | The source store could not be inspected, so the installed image was kept. Start the builder to check for and build changes. |
| `image '<ref>' was not found in the Docker Engine at <endpoint>` | Build it first, or fix `reference`. |
| `apple-local image source is not supported: Apple Containers <version> … is not a release awman has verified` | Install a supported Apple Containers release (see [Apple Containers store](#apple-containers-store)), or use another source. |
| `apple-local image source is blocked: the Apple Containers service is not running …` | Run `container system start`, then `awman ready`. |
| `apple-local image source is blocked: Apple Containers exists only on macOS …` | Use a `registry`, `docker-local` or `archive` source. |
| `image '<ref>' is not in the Apple Containers image store`, or `2 different Apple Containers images are named '<ref>'` | Build or pull it with Apple Containers, or remove the stale image. |
| `digest mismatch …`, `platform mismatch … wanted linux/arm64, found …` | A corrupt transfer, or the image was built for another architecture. Rebuild for the host platform. |
| `image archive <path> rejected: …` | The archive failed validation; the reason names the problem. |
| `not enough disk space at <path> …` | Free space on the state directory's filesystem. |
| `the builtin runtime cannot honour <request>: …` | Fractional CPU, out-of-range memory, or an unsupported agent option. |
| `Docker socket bridge is not supported on the builtin runtime` | You passed `--allow-docker`. Drop the flag; it is not available under `builtin`. |
| `cannot share <path> into the guest at <path>: only regular files and directories can be mounted; sockets such as the Docker socket are not bridged` | An overlay or mount points at a Unix socket, FIFO, or device node. Only regular files and directories can be shared into the VM. |
| `builtin exec output lagged; reconnect or restart the agent` | Your attached connection fell too far behind the live output and was closed rather than risk showing a false clean exit. Reconnect, or restart the agent if it isn't responding. |
| `cannot attach to <name>: its launching awman process has exited; …` or `cannot attach to <name>: no live attach endpoint (...); the launching awman must remain alive, and reattachment after its exit is unsupported` | The awman process that started this session has exited. Start a new session; reattaching to one whose launcher is gone is not supported. |
| `builtin runtime is busy: another awman process held …` | Retry shortly; another awman is importing an image or updating its catalog. |

See [Cleaning Up](13-cleaning-up.md#builtin-runtime-state) for reclaiming disk space.

---

## Docker Sandboxes (experimental)

`docker-sbx-experimental` runs each agent session in a dedicated microVM, providing hypervisor-grade isolation. It uses Docker's `sbx` CLI and is distinct from Docker Desktop — `sbx` is a standalone binary and does not require Docker Desktop to be installed.

This runtime is **experimental**. The `-experimental` suffix in the config string is intentional and durable: `sbx` itself has open known bugs as of June 2026 and its per-VM API is partly undocumented. The suffix will be removed in a future work item once both `sbx` and awman's integration have stabilized.

**Current status:** `awman ready`, `awman chat`, `awman exec`, and `awman exec workflow` all work end-to-end under `docker-sbx-experimental`. Run `awman ready` once to prepare the runtime, then use `awman chat` and `awman exec` exactly as you would with any other runtime.

### Platform support

| Platform | Status |
|---|---|
| macOS arm64 (Apple Silicon) | Supported |
| Windows x86_64 | Supported |
| Linux x86_64 | Blocked — a confirmed virtiofs bug (docker/sbx-releases#51, open as of June 2026) prevents agents from creating files in the workspace. Configuring this runtime on Linux returns a clear error. |

### What Docker Sandboxes is

Each sandbox is an isolated microVM with its own kernel (Linux 6.12 in Ubuntu), dedicated CPUs, private filesystem, and private Docker daemon. The agent binary runs natively inside the VM. The VM is created from a published template image pulled by host-side containerd — awman does not push any images to a registry.

The VM's private Docker daemon supports Docker-in-Docker, so agents that need to build and run containers work out of the box. The VM does not see your host Docker daemon, your host Docker images, or any host state outside the mounted workspace directory.

### How it differs from Docker and Apple Containers

| | Docker | Apple Containers | Docker Sandboxes |
|---|---|---|---|
| Isolation unit | Container (shared kernel) | Lightweight VM | MicroVM |
| Host escape requires | Container escape | Hypervisor CVE | Hypervisor CVE |
| Image model | Build from `Dockerfile.<agent>` | Same | Kit YAML — no custom OCI build or registry push |
| Volume mounts | `-v` bind mounts | `-v` bind mounts | virtiofs; workspace appears at the same absolute path inside VM |
| Env vars | `-e` flag or env file | Same | Credentials via `sbx secret set` + the credential proxy (not inherited from host shell) |
| Networking | Host bridge or NAT | Per-container network | All traffic through HTTP/HTTPS proxy; raw TCP/UDP blocked |
| Startup time | Milliseconds | ~1 second | 2–5 seconds cold (first launch per worktree) + startup script; subsequent restarts are faster |
| Stats | `docker stats` | `container stats` | Not reported — `agents status` and `agents detail` show no CPU or memory metrics; `agents list` never samples resource metrics |
| Persistence | Ephemeral (removed on exit) | Ephemeral | **Persistent** — sandbox state survives between sessions |

### Setup

**1. Install `sbx`**

```sh
brew install docker/tap/sbx   # macOS
```

For Windows, download from [Docker's releases page](https://docs.docker.com/go/sbx/).

**2. Authenticate**

```sh
sbx login
```

A free Docker account is sufficient.

**3. Run `awman ready`**

```sh
awman config set --global runtime docker-sbx-experimental
awman ready
```

`awman ready` for the sbx runtime:
- Verifies that `sbx` is on your PATH and that you are logged in.
- Emits per-agent kit files to `~/.awman/kits/<agent>/` for every configured agent.
- Validates each kit with `sbx kit validate`.

No credentials are registered at ready time — all secret registration happens at agent launch, scoped to the launched sandbox. See [Credentials](#credentials) below.

No images are built and nothing is pushed to a registry. `awman ready` for sbx is fast: kit emission and credential registration are text-file writes and short subprocess calls.

Every `sbx` command awman runs is announced in the status log before it executes. You can see exactly what awman is doing on your behalf.

### Kits — what they are and where they live

Instead of `Dockerfile.<agent>` files, the sbx runtime uses **kit YAML specs**. A kit is a directory containing a `spec.yaml` (which declares the base template image, install commands, network rules, and credential mappings) plus optional asset files (startup scripts, config templates).

Kits are generated by awman and stored globally, not per-repo:

```
~/.awman/kits/
├── claude/
│   ├── spec.yaml
│   └── files/home/.awman/apply-session-config.sh
├── codex/
│   ├── spec.yaml
│   └── files/home/.awman/apply-session-config.sh
└── …  (one directory per agent)
```

You do not edit these files by hand. Re-run `awman ready` to regenerate them (e.g., after upgrading awman or changing agent config). Use `awman ready --no-cache` to force a clean rebuild: awman removes any existing sandboxes for the affected agents and re-emits fresh kits. The next `awman chat` invocation pays the install cost again.

**Kit kinds:** Docker ships built-in sandbox templates for five agents (`claude`, `codex`, `gemini`, `copilot`, `opencode`). awman extends these as `kind: mixin` kits, which reuse Docker's published base and add awman-specific install and startup steps on top. For agents without a Docker built-in (`antigravity`, `crush`, `maki`, `cline`), awman emits `kind: agent` kits that build on a generic shell base image.

### Persistent sandbox lifecycle

Unlike Docker and Apple Containers, sbx sandboxes **persist between sessions**. The install cost (agent binary download, apt packages, etc.) is paid once per worktree-and-agent pair. After the first launch, subsequent `awman chat` calls restart the existing sandbox — the install does not re-run.

**First launch:**

```sh
awman chat --agent claude          # sbx creates the sandbox, installs the agent (one-time), attaches
```

awman runs `sbx create --kit ~/.awman/kits/claude --name awman-<hash>-claude claude <worktree>` (the worktree is the positional workspace path; it appears inside the VM at the identical absolute path), registers any sandbox-scoped secrets (see [Credentials](#credentials)), then attaches with `sbx run awman-<hash>-claude`.

**Subsequent launches (same worktree):**

```sh
awman chat --agent claude          # sbx restarts the existing sandbox, re-runs startup script, attaches
```

awman re-registers the sandbox-scoped secrets (so rotated keys apply), re-applies the per-launch session config (`sbx exec <sandbox> bash …/apply-session-config.sh` — the boot-time startup script only runs when the VM boots, so without this step a sandbox still running from a previous session would keep its old config and a changed `--model` would silently not apply), and runs `sbx run awman-<hash>-claude` (the existing sandbox is addressed by its positional name — `--name` is only valid when creating a new sandbox — and no `--kit` flag is passed, so the installed state is preserved).

**Teardown:**

```sh
sbx rm awman-<hash>-claude   # stops and removes the sandbox, clearing its persistent volume
```

**Listing running sandboxes:**

```sh
awman agents status        # shows awman-managed agents on the configured runtime
sbx ls                     # shows all sandboxes including non-awman ones
```

`awman agents status` includes a source marker for running agents. Regular user
sessions are marked `session`; containers launched by the squad daemon are
marked `squad:<task>` (for example, `squad:issue-triage`). The marker is
informational and may be visible whether or not you use squad.

**Sandbox naming:** awman names sandboxes `awman-<worktree-hash>-<agent>`. The hash is derived from the worktree's absolute path so the same worktree always produces the same name. Multi-agent workflows create one persistent sandbox per agent per worktree. When a workflow step runs, awman reuses the existing sandbox for that agent if one exists (restart, no reinstall), or creates a new one on first use. All steps that share the same agent share one sandbox — the per-agent install cost is paid once per worktree regardless of how many workflow steps use that agent.

### Credentials

The supported way to authenticate an sbx-backed agent is an `env(VAR)` overlay at launch time. Set the provider API key in your shell, then request it as an overlay:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
awman chat --agent claude --overlay 'env(ANTHROPIC_API_KEY)'
```

At agent launch (not `awman ready`), awman reads the value from your shell environment and registers it with a **sandbox-scoped** `sbx secret set <sandbox> <service>`, so the sbx host proxy is authenticated by the time the agent starts. On first launch awman creates the sandbox first (`sbx create --kit …`), sets the scoped secrets, then attaches with `sbx run <sandbox>`. awman never sets global (`-g`) secrets: scoped secrets take effect immediately (global ones only apply at sandbox creation), your keys never leak into sandboxes awman doesn't manage, and removing a sandbox removes its secrets with it. Because rotated keys are re-registered on every launch, you never need to re-run `awman ready` after changing a key.

Each agent accepts a specific allowlist of auth variables (matching the [Docker Sandboxes credential services](https://docs.docker.com/ai/sandboxes/security/credentials/)):

| Agent | Supported env vars | sbx service |
|---|---|---|
| `claude` | `ANTHROPIC_API_KEY` | `anthropic` |
| `codex` | `OPENAI_API_KEY` | `openai` |
| `gemini` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | `google` |
| `copilot` | `GH_TOKEN` or `GITHUB_TOKEN` | `github` |
| `opencode` | `ANTHROPIC_API_KEY` | `anthropic` |

Rules:

- An `env(VAR)` overlay for a credential-class variable **not** on the agent's allowlist is dropped with a warning (it is never written anywhere workspace-readable).
- Launching with no supported auth overlay does not warn blindly. awman first probes for a pre-existing sbx secret that already covers the agent's provider — it runs `sbx secret ls --service <service>` (the well-known service name, e.g. `anthropic` for `claude`) and `sbx secret ls -g` (the global store). If a matching secret is found (from a previous launch, a manual `sbx secret set`, or a global key), awman prints an informational note that the existing credential will authenticate the agent and launches normally. Only when no pre-existing secret is found does it warn that auth must be configured manually (`sbx secret set <sandbox> <service>` — sandbox-scoped secrets apply immediately, even to a running sandbox) or via the agent's own login flow inside the sandbox — and the launch still proceeds either way.
- Custom-kit agents (`antigravity`, `crush`, `maki`, `cline`) do not participate in launch-time auto-auth yet; register their credentials manually with `sbx secret set`.
- Non-credential `env(VAR)` overlays (e.g. `LOG_LEVEL`) are unaffected — they reach the agent through `session.json` as before.

Credentials are piped to `sbx secret set` via stdin — they never appear in process listings, and they are masked in all awman output.

**Claude subscription tokens:** Docker Sandboxes does not yet support `CLAUDE_CODE_OAUTH_TOKEN` (the token from `claude setup-token`), so awman silently skips it — no warning is printed, since the token is surfaced on every launch whenever you are logged in to Claude Code and auth works without it. To use Claude Code under the sbx runtime, either set `ANTHROPIC_API_KEY` (the missing-auth warning above still fires when no supported variable is configured), or run `/login` inside the sandbox on first launch — the sbx credential proxy completes the OAuth flow and keeps the resulting token on the host. This is unchanged by, and unaffected by, the live-refreshed credential file awman writes for Claude under the Docker and Apple Containers runtimes (see [Agent authentication](03-agent-sessions.md#agent-authentication)) — sbx sessions still authenticate only through `ANTHROPIC_API_KEY` or in-sandbox login.

Non-credential config (model selection, mode flags, system prompts, tool lists) is written to `<workspace>/.awman/session.json` before each launch and read by the startup script inside the VM. Credential values are never written to `session.json`.

### Session config and seeded prompts

How awman delivers a seeded prompt (e.g., from `awman exec prompt --agent claude "fix the bug"`) depends on the kit kind:

| Kit kind | Prompt delivery |
|---|---|
| `kind: mixin` (`claude`, `codex`, `gemini`, `copilot`, `opencode`) | Written into the agent's stdin at launch (typed ahead for interactive sessions, piped for `--non-interactive`) |
| `kind: agent` (`antigravity`, `crush`, `maki`, `cline`) | Appended as a positional argument at launch |

Mixin kits launch their agent through Docker's built-in sandbox template, so awman cannot put the prompt on the command line — stdin injection is the single delivery path. The prompt is also recorded in `session.json` and staged by the startup script at `~/.awman/seeded-prompt.txt` inside the VM for reference, but that copy is informational, not a second delivery.

Mixin startup scripts consume these `session.json` fields where the agent supports them natively:

- `model` — sets the agent's default model
- `permission_mode` — `--plan` / `--auto` / `--yolo`; claude renders it as `permissions.defaultMode` (`plan` / `acceptEdits` / `bypassPermissions`) in its settings file. For `--yolo`, the script also sets `skipDangerousModePermissionPrompt: true` so claude does not ask for interactive confirmation of bypass-permissions mode — the sandbox is the isolation boundary, matching the container runtimes' behavior
- `system_prompt_inline.text` — applies an inline system prompt via the agent's native config
- `allowed_tools` / `disallowed_tools` — Claude applies tool allow/deny lists through its native permissions settings. Other sandbox agents reject these lists before launch because their startup scripts cannot apply them.

If a mixin agent cannot express a field through its native config, awman prints a warning at launch time. Tool allow/deny lists instead refuse the launch when they cannot be applied. `kind: agent` kits do not consume these fields from `session.json` — configuration is delivered as launch arguments appended after the `--` delimiter, mirroring the container runtimes' argv: the non-interactive subcommand/flag (e.g. `crush run`, `cline task`), mode flags (`--yolo`, …), the model flag (`--model <NAME>`), then the seeded prompt. (`antigravity` delivers system prompts through extra directory mounts under the container runtimes; those mounts don't exist under sbx, so awman warns that the system prompt was not applied.)

No prompt is delivered twice: mixin kits use stdin; `kind: agent` kits use positional arguments; neither path uses both.

### Supported agents

All nine awman agents are supported under `docker-sbx-experimental`:

| Agent | Kit kind | Notes |
|---|---|---|
| `claude` | mixin | Extends `docker/sandbox-templates:claude-code-docker` |
| `codex` | mixin | Extends `docker/sandbox-templates:codex-docker` |
| `gemini` | mixin | Extends `docker/sandbox-templates:gemini-docker` |
| `copilot` | mixin | Extends `docker/sandbox-templates:copilot-docker` |
| `opencode` | mixin | Extends `docker/sandbox-templates:opencode-docker` |
| `antigravity` | agent | Full install on `shell-docker` base |
| `crush` | agent | Full install on `shell-docker` base |
| `maki` | agent | Full install on `shell-docker` base |
| `cline` | agent | Full install on `shell-docker` base |

### Known limitations

**Platform:** macOS arm64 and Windows x86_64 only. Linux is blocked until docker/sbx-releases#51 is fixed upstream.

**Networking:** all traffic goes through an HTTP/HTTPS proxy. Raw TCP, UDP, and ICMP are blocked. This means SSH-based git remotes, database connections, and other raw-socket protocols do not work inside the VM by default. Agents that need npm, pip, or curl to reach the internet will work; agents that need to SSH into a server will not.

**No per-resource stats:** `awman agents status` and the TUI status panel show running sandboxes from `sbx ls`, but cannot report per-sandbox CPU or memory usage. The `docker stats`-style live resource view is unavailable for sbx sandboxes.

**`--allow-docker`:** Docker-in-Docker is always on inside an sbx sandbox (each VM has a private Docker daemon). The `--allow-docker` flag (which mounts the host daemon socket into Docker containers) is a no-op under sbx — it is ignored (noted in the debug log).

**Directory overlays, skills, and context mounts:** the VM can only see the workspace, which is virtiofs-mounted at sandbox creation. `dir(...)` overlays, skill mounts (`--include-all-skills` / `skill(...)`), and context directory mounts (`context(...)`) cannot be honored — awman warns at launch and continues without them. For `context(...)`, the rendered system prompt text is still delivered to the agent; only the directory mount is skipped.

**Mixin launch flags:** mixin kits launch the agent through Docker's built-in template, whose launch command takes no CLI flags from awman. Anything with a native config-file equivalent is delivered through `session.json` (see [Session config](#session-config-and-seeded-prompts)); the rest is warned about at launch instead of silently dropped:

| Option | claude | codex / gemini / opencode | copilot |
|---|---|---|---|
| `--model` | applied (settings file) | applied (settings/config file) | **not deliverable** — warned; use the `/model` slash command in-session |
| `--plan` / `--auto` / `--yolo` | applied (`permissions.defaultMode`) | **not deliverable** — warned | **not deliverable** — warned |
| `--non-interactive` | **not deliverable** — warned; the prompt is still piped to the agent's interactive entrypoint | same | same |
| `agentFlags` / `--agent-flag` | **not deliverable** — warned | **not deliverable** — warned | **not deliverable** — warned |

`kind: agent` kits (`antigravity`, `crush`, `maki`, `cline`) are unaffected: awman owns their entrypoint and passes mode flags, `--model`, `agentFlags`, and the non-interactive subcommand as launch arguments, exactly like the container runtimes. (`antigravity` rejects `--model` on every runtime — it has no model flag.)

**Port mappings:** if a workflow binds a port inside the sandbox, that port mapping is lost when the sandbox stops. Port-based workflows must keep the sandbox running continuously.

**Template drift:** if Docker updates a built-in sandbox template (e.g., changes the default entrypoint for `claude-code`), existing sandboxes keep the old template until they are destroyed and recreated. Run `awman ready --no-cache` to re-emit kits for the new template; existing sandboxes must be removed with `sbx rm` before the next launch picks up the new kit.

### Troubleshooting

**`sbx` not found:**
```
awman: 'sbx' not found on PATH. Install with: brew install docker/tap/sbx
```
Install `sbx` and re-run `awman ready`.

**Not logged in:**
```
awman: sbx ls failed — run 'sbx login' to authenticate Docker Sandboxes
```
Run `sbx login` and re-run `awman ready`.

**Linux:**
```
awman: docker-sbx-experimental is not supported on this platform
```
Use `runtime: "docker"` or `runtime: "apple-containers"` instead.

**Stale kit version:**
```
awman: session.json schema_version mismatch — re-run 'awman ready'
```
The kit on disk was emitted by an older version of awman. Run `awman ready` to regenerate.

**Kit validation failure:**
```
awman: sbx kit validate failed for ~/.awman/kits/claude: <error from sbx>
```
The error text from `sbx kit validate` is printed verbatim. Common causes are a stale `sbx` binary (upgrade it) or a network issue during template pull. Run `awman ready --no-cache` to force a clean re-emit.

**Launch failure:**
```
sbx exited with code 1; last output:
ERROR: failed to create sandbox: <error from sbx>
```
When `sbx run` exits non-zero, awman replays the tail of sbx's output into the execution window alongside the exit code, so the failure is diagnosable without re-running the announced `sbx` command by hand.

---

## Switching runtimes

You can switch between runtimes at any time by changing the `runtime` setting. Each runtime maintains its own state:

- **Docker / Apple Containers:** per-repo `.awman/Dockerfile.<agent>` files and the local Docker / containerd image store. These are unchanged when you switch to sbx.
- **Docker Sandboxes:** `~/.awman/kits/<agent>/` (host-global kit files) and `<workspace>/.awman/session.json` (per-launch dynamic config written just before each launch). These are not touched when you switch to Docker or Apple Containers.

- **Builtin:** the private state directory (`~/.awman/builtin` by default) holding imported images and session VMs. Its images are separate from the Docker and Apple image stores.

Switching runtimes does not delete state for the other runtimes. You can keep all four ready simultaneously.

**Switching mid-project:** if you run `awman chat` under Docker and then switch to sbx and run `awman chat` against the same worktree, you start a fresh sbx sandbox. In-VM state from the Docker container is not transferred (containers and microVMs are completely separate environments). Your Git repo state is shared — both runtimes see the same files on disk.

**Example: switch and switch back**

```sh
# Set up Docker (already done at init)
awman config set --global runtime docker
awman ready

# Try the sbx runtime
awman config set --global runtime docker-sbx-experimental
awman ready                         # emits kits, registers credentials
awman chat --agent claude                   # runs in sbx sandbox

# Switch back to Docker — Docker images are still there
awman config set --global runtime docker
awman chat --agent claude                   # runs in Docker container, unchanged
```

`awman ready` is per-runtime: running it with `runtime: "docker"` does not touch sbx kits; running it with `runtime: "docker-sbx-experimental"` does not touch Docker images; running it with `runtime: "builtin-experimental"` imports into the builtin store and builds only when the opt-in builder and a compatible local source schedule a build.

---

[← GitHub Integration](10-github-integration.md) · [Next: squad →](12-squad.md)

The builtin runtime is powered by Microsandbox, embedded in awman; no separate installation is required.
