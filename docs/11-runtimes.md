# Runtimes

awman v0.14 supports Docker, Apple Containers, and the builtin microVM runtime. The runtime controls how agent processes are isolated from your host machine. These use the same `awman` commands, workflow files, and agent names. The `sbx` configuration value is recognized but disabled for this release.

Preparation output follows the same rules on every runtime: reusing a ready
image is quiet, and progress describes work that is needed. Docker and Apple
Containers show build progress only when building. Builtin shows a short
`Preparing <image> (<source>)` status, which may reuse cached content. Warnings
and failures remain visible, and `ready` finishes with a readiness summary.
Build output is printed without a `running` suffix on every line.

---

## Choosing a runtime

| Config value | Platform | Isolation unit | Requires |
|---|---|---|---|
| `docker` (default) | Linux, macOS, Windows | Linux container (shared kernel) | Docker daemon |
| `apple` | macOS 26+ only | Lightweight VM per container | macOS 26 Tahoe |
| `sbx` | Disabled in v0.14 | No sessions can start | See [issue #86](https://github.com/prettysmartdev/awman/issues/86) |
| `builtin` | macOS on Apple Silicon and Linux x86_64 with KVM (experimental) | MicroVM per session, embedded in awman | The `awman-macos-arm64` or `awman-linux-amd64` release binary, or a supported source build. No Docker, `container`, or `sbx` needed to *run* agents |

Set the runtime in your global config (`runtime.kind` is a global-only key):

```sh
awman config set runtime.kind docker                   # default
awman config set runtime.kind apple         # macOS 26+ only
awman config set runtime.kind builtin     # experimental, Apple Silicon or Linux x86_64 with KVM; see below
```

You can switch among the supported runtimes at any time — each keeps its own separate state and does not interfere with the others. See [Switching runtimes](#switching-runtimes).

[Custom agents](15-custom-agents.md) (including `crush` and `maki`) run on `docker`, `apple` and `builtin`.

**Invalid values are a fatal error.** If `runtime.kind` holds anything other than the four recognized values above (including the spellings earlier betas used), awman does not fall back to Docker, because launching agents under a different isolation model than the one you configured would be unsafe. Commands that would run an agent print the invalid value and the valid options, then exit; the interactive TUI opens a modal dialog on startup with the same message (press Enter to quit). `awman config …` keeps working so you can fix it (`awman config set runtime.kind docker`), or edit the global config file directly.

---

## Docker (default)

Docker is the default runtime on all platforms.

**Requirements:** Docker daemon running.

**Isolation model:** standard Linux containers. The agent runs in a container with your project mounted read-write. The container shares the host kernel; host escape requires a container escape exploit.

**Lifecycle:** containers are ephemeral. The agent container is removed when the session ends. Persistent state lives in your Git repository.

**`awman ready` for Docker** builds the project image from `Dockerfile.dev` and the per-agent images from the `.awman/Dockerfile.<agent>` files. Images are rebuilt when you run `awman ready --build` or when another normal ready rule schedules a build; `--no-cache` disables build cache for builds that run.

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

## Keeping images fresh (Docker and Apple Containers)

On the runtimes that build images locally (`docker` and `apple`), awman labels every project and agent image it builds with a hash of what went into it, and compares that hash with your working tree before a session or workflow starts.

| Label | Value |
|---|---|
| `dev.awman.inputs` | Hash of the inputs the image was built from. |
| `dev.awman.repo` | Normalised repository key: the `origin` URL without scheme, credentials, default port, `.git` or trailing `/` (such as `github.com/org/repo`), or a short path hash when there is no remote. |

The hash covers:

- **Project image:** the project Dockerfile (`Dockerfile.dev`, or the file named by `runtime.dockerfile`) and the content of every file or directory the Dockerfile `COPY`s or `ADD`s from the build context. File contents, paths and executable permissions count; modification times do not. The root `.dockerignore` and the selected Dockerfile’s `.dockerignore` file also count, so changes to exclusions trigger a rebuild.
- **Agent image:** the `.awman/Dockerfile.<agent>` file and the ID of the project image it was built on, so rebuilding the project image rebuilds the agent images on top of it.

Some sources cannot be resolved to files, so only the Dockerfile text itself covers them: wildcards (`*`, `?`, `[`), variables (`$`), URLs, paths that escape the context, a whole-context copy (`COPY . .`, which would otherwise rebuild on every change), more than 10,000 files in one directory, more than 512 MiB in total, and unreadable paths. `COPY --from=` is ignored, symlinks are hashed by their link target text and never followed, and ignore patterns are not applied to the files being hashed (so excluded files can cause an extra rebuild). `ONBUILD` instructions and `RUN --mount` inputs are not resolved. Agent hashes do not include files copied from the context by an agent Dockerfile; use `ready --build` after changing those files.

### When images are rebuilt

During API session setup, before `chat`, `exec prompt` and squad tasks launch, and during workflow preflight, awman compares the installed images' `dev.awman.inputs` labels with the current hash:

- A **stale** image (the label differs) is rebuilt: the project image first, then each agent image.
- An **unlabelled** image, built by an earlier awman, is rebuilt once and is labelled from then on.
- A **missing** image behaves as before: a missing agent image is built, and a missing project image tells you to run `awman ready`.
- Concurrent awman processes share one rebuild: a second process waits for the first and reuses its image.

The workflow prerequisite review checks freshness as well as existence. An image
that needs rebuilding is shown as requiring preparation, with the reason. This
also applies when its project image needs rebuilding or its generated agent
Dockerfile will change. `--check-prerequisites` reports these pending actions
without writing Dockerfiles, downloading, building or importing images. If a
refresh fails and awman continues with the installed image, the final report
retains that warning instead of declaring the image satisfied.

Image tags are named after the repository folder, so alternating between branches whose Dockerfiles or copied files differ rebuilds the image each time you switch.

If a rebuild fails, awman **warns and continues** with the image it already has. API clients receive an `image_stale` event. `awman ready --build` still fails when its build fails. Plain `awman ready` does not rebuild stale images. With an enabled Docker-local or Apple-local builder, builtin session setup can also rebuild stale source images before import; a failed rebuild keeps the installed image. Registry/archive sources and disabled builders retain their import behavior.

### Content awman cannot see

The hash sees only files in your repository. Anything fetched while the image builds, such as `curl … | sh`, an unpinned `apt-get install` or a moving base-image tag, can change without changing the hash, so awman does not rebuild for it. Pin such content, or force a rebuild yourself:

```sh
awman ready --build              # rebuild, using the build cache
awman ready --build --no-cache   # rebuild from scratch
```

---

## Builtin microVM (experimental)

`builtin` runs each agent session in a Linux microVM whose VM code, kernel, and guest agent are **embedded in the awman executable** (it is powered by Microsandbox). Each VM starts another process of that same executable. There is no separate runtime to install, and no Docker, `container` or `sbx` is needed to *run* agents.

**Isolation model:** a hardware-virtualized VM with its own kernel per session. The hypervisor, device emulation, and shared-filesystem implementations form part of the security boundary. The configured project, settings, and overlay mounts grant access to host files. There is no host Docker socket bridge.

> **Experimental, macOS on Apple Silicon and Linux x86_64 with KVM.** This runtime is released as an experimental preview: behaviour and configuration may still change. Linux requires a GNU/glibc x86_64 build and read/write access to `/dev/kvm`; see [Parity limits](#parity-limits-versus-docker-and-apple-containers) for what differs from the other runtimes.

### Where it is available

| Host | Status |
|---|---|
| macOS on Apple Silicon | Supported (experimental). Included in the `awman-macos-arm64` release and in source builds (`make build`). |
| Linux x86_64 (GNU/glibc) | Supported (experimental) with KVM. Included in `awman-linux-amd64` (glibc 2.35 or newer) and source builds (`make build`). |
| Linux arm64 | Not supported yet (gated until native acceptance). Builds do not include the runtime. Other supported runtimes remain available. |
| Linux musl, Windows | Builtin runtime unavailable. Other supported runtimes remain available. |

The runtime is present only in awman builds compiled with it. The `awman-macos-arm64` and GNU/glibc `awman-linux-amd64` release assets include it. Linux arm64, musl and Windows builds do not. A build without it reports `the builtin runtime is unavailable: this build of awman does not include the builtin-runtime feature`.

### Enabling it

The runtime is only ever used when you select it explicitly in your **global** config (`~/.awman/config.json`). It is never a default and a repository config cannot select it:

```sh
awman config set runtime.kind builtin
awman ready
```

or edit the global config file (`~/.awman/config.json`) directly:

```json
{ "runtime": { "kind": "builtin" } }
```

The value is `builtin`; the earlier `builtin-experimental` spelling is no longer accepted. `awman config set` refuses unknown runtime values. If the file already holds one, every command that would run an agent refuses to start (awman never falls back to Docker). Edit a malformed file directly; for a valid file naming a runtime this host cannot use, repair it with a startup override, for example `awman --set runtime.kind=docker config set runtime.kind docker`.

`runtime.kind` and `runtime.builtin.stateDir` are fixed for a running awman process: a TUI or API command that asks for a different value is refused with `process_scoped_setting` (HTTP 409) and tells you to restart with the value you want. VM size, network and image-source settings stay per session. See [Settings fixed for an awman process](07-configuration.md#settings-fixed-for-an-awman-process).

### Linux KVM access

The builtin runtime needs hardware virtualization exposed as `/dev/kvm`. On a
Debian or Ubuntu host, grant your account access and log out and back in:

```sh
sudo usermod -aG kvm "$USER"
# After logging in again:
test -r /dev/kvm && test -w /dev/kvm
awman config set runtime.kind builtin
```

A cloud VM also needs nested virtualization from its provider. If `/dev/kvm`
is missing or inaccessible, awman reports the prerequisite and refuses to start
a guest. Running awman as root is unnecessary.

For image preparation, choose an archive, registry, or Docker Engine source.
To build project images locally, install Docker, allow your account to use its
Unix socket, and enable `runtime.builtin.builder.enabled`. Docker prepares
and exports the image; cached agent execution uses KVM and needs no Docker
service. Apple Containers sources are available only on macOS.

### The macOS binary, Gatekeeper and permissions

macOS only lets a program start a virtual machine when its code signature carries Apple's `com.apple.security.hypervisor` entitlement. The `awman-macos-arm64` binary is therefore **ad-hoc signed** with exactly that entitlement. It is **not** signed with an Apple Developer ID and **not** notarized. What that means for you:

- **Gatekeeper and quarantine.** A file downloaded with a browser, or received by AirDrop, Mail or Messages, is marked as quarantined, and macOS refuses to open it ("cannot be opened because the developer cannot be verified"). Installing with the installer (`curl -s https://prettysmart.dev/install/awman.sh | sh`), or downloading with `curl` or `gh release download`, does not add the quarantine mark. If you downloaded with a browser, first verify the file against the published checksum (below), then remove the mark:

  ```sh
  xattr -d com.apple.quarantine ./awman-macos-arm64
  ```

- **Verify the checksum and attestation.** An ad-hoc signature proves the file has not changed since it was signed, not who signed it. Every release publishes a `SHA256SUMS` file next to the executables. Download it into the same directory as the executable and compare:

  ```sh
  shasum -a 256 -c SHA256SUMS --ignore-missing
  ```

  On Linux use `sha256sum -c SHA256SUMS --ignore-missing`. You can also compute the digest yourself (`shasum -a 256 awman-macos-arm64`) and compare it with the line for your file in `SHA256SUMS` or on the release page.

  Releases built by awman's release pipeline also carry a GitHub build provenance attestation for each executable, which proves the file was built from this repository's source by its release workflow and has not changed since. Verify it with the [GitHub CLI](https://cli.github.com/):

  ```sh
  gh attestation verify awman-macos-arm64 --repo prettysmartdev/awman \
    --signer-workflow prettysmartdev/awman/.github/workflows/release.yml
  ```

  The same command works for `awman-linux-amd64`, `awman-linux-arm64` and `awman-windows-amd64.exe`. A successful run prints the repository, workflow and commit the file was built from; a failure means the file is not the published release, so delete it and download it again. If the command reports that no attestations were found, the release predates the release pipeline, and the checksum is the check available for it. For an older release signed by a different workflow, rerun the command without `--signer-workflow` and check the workflow it reports. A checksum match does not replace the attestation: `SHA256SUMS` is published by the same release, whereas the attestation is signed by GitHub.

- **Don't modify the binary.** Anything that changes the file after signing (for example `strip`, patching, or a tool that relinks it) invalidates the signature, and macOS then refuses to run it or to start VMs from it. Re-sign it (see *Building from source*) or download it again.
- **Folder-access prompts.** macOS asks before a program reads `~/Desktop`, `~/Documents`, `~/Downloads`, iCloud Drive or removable volumes. When you run awman from a terminal, that permission belongs to your terminal app, so upgrading awman changes nothing. The **squad daemon** runs in the background under launchd, so there the permission belongs to awman itself — and because the signature is ad-hoc, macOS treats every new awman version as a new program. If a squad task works in a repository under one of those folders, macOS asks again after each awman upgrade, and the task waits until you answer. Keep squad workspaces elsewhere (for example `~/Workspaces`) to avoid this, or review the grants in **System Settings → Privacy & Security → Files and Folders**. The keychain is not affected: awman reads credentials through Apple's own `security` tool, so keychain access does not change when awman does.

### Building from source

Run `make install`. On Linux x86_64 GNU and Apple Silicon, `make build`/`make install` fetch and checksum-verify the embedded kernel and guest agent automatically and include the builtin runtime. Apple Silicon builds are also **ad-hoc signed with the hypervisor entitlement automatically** (`tools/macos-sign.sh`). `make install` builds as your user, uses `sudo` only if the install directory requires it, and atomically replaces the installed executable. On macOS it verifies the installed signature. Other hosts build the existing runtimes only.

On macOS, if you copy, strip or otherwise change the binary afterwards, sign it again:

```sh
make sign-macos BIN=/usr/local/bin/awman
```

An unsigned build fails to start agents with `embedded worker startup failed; verify KVM access or the macOS com.apple.security.hypervisor entitlement …`.

### Getting an agent image

Docker and Apple Containers build `awman-<project>:latest` and `awman-<project>-<agent>:latest` from `Dockerfile.dev` and `.awman/Dockerfile.<agent>`. By default the builtin runtime **imports** finished images from a source you choose; you can opt into source-matched local builds with `runtime.builtin.builder.enabled` (see [Building from a local image source](#building-from-a-local-image-source)). Without that setting, build the images yourself and tell awman where to import them from.

**On a Mac, the simplest route is Apple Containers.** If you already use the `apple` runtime, the images are already in its store: set `"imageSource": {"type": "apple-local"}` and run `awman ready` (see [Apple Containers store](#apple-containers-store)). Otherwise build them with `container build` (same arguments as `docker build` below) and use the same source. Docker, a registry, or an archive file work too:

**1. Build the images** from the repo root, in this order (the agent image is `FROM` the project image). Running `awman ready` first prints these exact commands for your project and agent, and downloads `.awman/Dockerfile.<agent>` if it is missing:

```sh
docker build -t awman-myproject:latest -f Dockerfile.dev .
docker build -t awman-myproject-claude:latest -f .awman/Dockerfile.claude .
```

Build for the host's architecture (`linux/amd64` on x86_64 hosts, `linux/arm64` on ARM hosts, including Apple Silicon). The build can happen on another machine.

**2. Choose an image source** in `runtime.builtin.imageSource` (or per image in `runtime.builtin.images`) in your global or repo config JSON:

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

Readiness checks stay quiet when an installed image can be reused. If preparation
is needed, you see a short status such as
`Preparing awman-myproject-claude:latest (apple-local)`. Preparation may reuse
cached content; it does not necessarily download or import the image again.
Warnings and failures still include the details needed to act on them.

There is no default source. In an interactive CLI or TUI session, `awman ready` offers to configure one when the requested builtin image has no usable source. In headless or other non-interactive flows, it does not guess a registry or daemon: readiness fails with a message naming the missing config fields (see [Interactive image-source setup](#interactive-image-source-setup)).

**`awman init` and `builtin`.** `awman init` builds images through the configured runtime, so with `runtime.kind` set to `builtin` its build step fails (`image building; build externally and import with ready is not supported on the builtin runtime`) and the agent audit is skipped. Run `awman init` before selecting `builtin` (or with `docker` or `apple` temporarily selected), then switch the runtime and run `awman ready` after building externally.

### Interactive image-source setup

When an interactive CLI or TUI `awman ready` run finds that its builtin image has no configured source, it offers these choices:

- **docker-local** — import an image already present in the selected local Docker Engine.
- **apple-local** — on macOS, import an image already present in the Apple Containers store.
- **archive** — import an image from a saved image archive.
- **registry** — pull an image from an OCI registry.
- **Leave unconfigured** — make no change and let this `ready` run continue with the missing-source result (an already-installed image remains usable).

Importing reads an existing image from the selected store, archive, or registry. It does not build an image. For docker-local and apple-local, `ready` separately offers an explicit builder opt-in when that store supports building and the builder is not already enabled. The default answer is no. Accepting it sets `runtime.builtin.builder.enabled` to `true`; the source selection and builder choice are shown in a preview before any file is written.

The prompt lets you choose the repository config (`<git_root>/.awman/config.json`) or global config (`$AWMAN_HOME/config.json`, normally `~/.awman/config.json`) when both are available. It displays the exact JSON change and the resulting effective source configuration, and saves only after confirmation. After saving, the same `ready` run reloads configuration and continues image preparation; there is no need to run `ready` a second time. With `--build`, `ready` also considers other agent images; a source selected for a non-default agent is saved under that image's tag in `runtime.builtin.images` so a one-image archive or fixed reference does not get applied to other agents.

No source setup prompt is shown for `--headless`/`-n` or `--json`, or for API, workflow, or squad preparation. Those paths do not write source configuration. `ready`, API session setup, and workflow prerequisite errors name `runtime.builtin.imageSource` or the tagged field ``runtime.builtin.images["<tag>"]`` and explain which one to set. Squad image preparation also does not prompt or write source configuration; run `awman ready` in a terminal to set up and prepare a missing image. A source that is configured but fails to import is reported as an import failure and does not trigger this setup prompt.

For example, a headless run with an unconfigured `awman-myproject-codex:latest` image reports that no image source is configured for that tag and agent, then tells you to set `runtime.builtin.imageSource` (the default source for every agent image) or ``runtime.builtin.images["awman-myproject-codex:latest"]`` (for this image only). It includes the config file paths and accepted source types. If you also want `ready` to build into a supported local store, set `runtime.builtin.builder.enabled` to `true` separately.

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

**Per-image overrides.** `runtime.builtin.images` maps an awman image tag to its own source; it wins over `runtime.builtin.imageSource` for that tag:

```json
{
  "runtime": { "builtin": {
      "imageSource": { "type": "docker-local" },
      "images": {
        "awman-myproject-claude:latest": { "type": "archive", "path": "/opt/images/claude.tar" }
      }
  } }
}
```

**Registry settings.** `runtime.builtin.registries` is keyed by `host[:port]`:

```json
{
  "runtime": { "builtin": {
      "registries": {
        "registry.example.com": {
          "caCert": "/etc/ssl/corp-ca.pem",
          "auth": { "type": "env", "usernameVar": "REG_USER", "passwordVar": "REG_PASS" }
        },
        "localhost:5000": { "insecure": true }
      }
  } }
}
```

| Key | Meaning |
|---|---|
| `insecure` | `true` allows plain HTTP for this host only (default `false`) |
| `caCert` | PEM bundle of extra trusted CAs for this host |
| `auth` | Where credentials come from; secrets are never stored in config. `{"type":"anonymous"}`; `{"type":"env","usernameVar":…,"passwordVar":…}` reads named environment variables; `{"type":"keychain","service":…}` reads an item from awman's OS keychain (account = registry host, value `username:password`); `{"type":"docker-config"}` reads inline `auths` entries from `~/.docker/config.json` |

`docker-config` supports inline credentials only. If the file names a `credsStore` or `credHelpers` entry, awman reports an authentication error naming the helper and does **not** execute it; use `env` or `keychain` instead. Credentials are sent only to the registry itself (and Docker Hub's own token service); a token server on any other host is contacted anonymously. Registry pulls honor `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY`.

### What `awman ready` does under `builtin`

- With the builder disabled, the base project image is reported as **skipped** (it is only an input to your external build). With the builder enabled and a compatible local source, it is built in that source store first (see [Building from a local image source](#building-from-a-local-image-source)). The agent image is **imported**, and an **Image source** row appears in the summary (also in `--json` output).
- An image is re-acquired when the configured source differs from the installed image's source, or when a successful build is waiting to be imported; otherwise the installed or cached copy is reused. With `runtime.builtin.builder.enabled` and a compatible local source, session setup, launches, workflow prerequisite solves, worker leases and squad also compare source input hashes and build missing or stale images before importing. Use `awman ready --build` to force a refresh even when those inputs match. The `Dockerfile` path used for builds and hints is the repo's configured one.
- With the builtin builder enabled, a chat or prompt launch whose agent image is not installed prepares it first, as Docker and Apple Containers do: it builds the base and agent images in the source store if that store lacks them (fetching `.awman/Dockerfile.<agent>` if needed), then imports the result. So `awman chat --agent codex` works without a separate `ready` run. With an incompatible source (a registry, an archive, a remote Docker endpoint) the launch imports from it without building. With the builder disabled, a launch never builds or imports: run `awman ready` first.
- With an enabled local builtin builder, chat and prompt launches check source input hashes, the installed image's recorded source and any pending build receipt. A current image from the same source is reused without exporting, acquiring or importing it again, and without import progress messages. Missing or stale source images are built and imported as needed. Other sources keep their check-only behavior for installed images; run `awman ready` if their recorded source differs or provenance is missing.
- With a configured source, `ready` checks the acquisition cache using the source kind, resolved endpoint or archive path, reference, and platform. Changing any of these selects that source's cache entry or acquires it. A replaced archive is revalidated; a cached archive remains usable after its source is removed. With no source configured, an installed image is reused.
- With the builder disabled (the default), `awman ready --build` and `--no-cache` **re-acquire** the image from its source; they do not build. `--refresh` (the Dockerfile audit) is unchanged. With the builder enabled, `--build` schedules the usual builds, and `--no-cache` applies to any build that runs and still re-acquires the image from its source.
- `awman ready --build` also prepares the image of **every other agent** that has a `.awman/Dockerfile.<agent>`, with one preparation status per image, so `chat --agent`, workflow steps, and other agents can run without a separate import. Plain `awman ready` prepares only the default agent and lists any other agents whose images are missing.
- Other agents use their own `runtime.builtin.images` entry when there is one, otherwise `runtime.builtin.imageSource`. A `runtime.builtin.imageSource` that names a single image — an `archive`, or any source with an explicit `reference` — holds only one agent's image, so for other agents `ready` skips it with a warning instead of importing the wrong image. Give each of those agents its own `runtime.builtin.images` entry (for example one archive per agent). Workflow preflight and squad apply the same rule: a step running another agent fails with that warning's text rather than importing the default agent's image under its tag.
- When the summary reports agent images that are still missing, its closing hint says what to do for each one: `run "ready --build" to import <agents>.` (or `build <agents>` / `build and import <agents>`, depending on what each image's source allows), and ``configure an image source for <agents> (`runtime.builtin.imageSource` or a `runtime.builtin.images` entry), then run "ready --build".`` for an agent with no usable source. A digest-pinned local reference or colliding build output instead asks you to configure a writable, distinct reference in `runtime.builtin.images` before running `ready --build`. A failed image gets its own hint, for example `fix the <agent> image build, then run "awman ready --build" again.` or `the <agent> image was built but not imported; run "awman ready" again to retry the import.`
- If the `--refresh` audit changes `Dockerfile.dev` and this session cannot rebuild the default image, `ready` warns: ``Dockerfile changed: rebuild the image in its source store (or enable `runtime.builtin.builder.enabled` with a `docker-local` or `apple-local` source), then run `awman ready --build` to re-import it``, or, with no source configured, to rebuild it externally and configure `runtime.builtin.imageSource`.
- Older acquisition-cache references without per-source metadata must be acquired again once. Installed images remain runnable without their source.
- A transient failure while importing (a dropped connection, a stalled transfer, a 5xx from the registry or daemon) is retried automatically, up to a bounded number of attempts and an overall time limit. Errors that won't be fixed by retrying — bad credentials, a digest or platform mismatch, a rejected archive, an unsupported or blocked source — are reported immediately instead. Ctrl-C cancels acquisition and waits for cleanup. If the final runtime import has already begun, cancellation waits for that commit and may leave a complete cached image. Local filesystem or keychain calls may delay the response.
- Workflow setup/teardown steps use the imported agent image of the repo's configured agent (default `claude`) unless the workflow or config sets an explicit `runtime.baseImage`, which must already be imported. Missing agent images are prepared in the [workflow prerequisite flow](05-workflows.md#workflow-prerequisite-checks). Headless runs prepare automatically under the same builder settings; `--check-prerequisites` remains read-only.

### Building from a local image source

Set `runtime.builtin.builder.enabled` to `true` to let `awman ready`, launches and workflow prerequisite solves (including worker leases and squad) build missing images and import them while keeping `builtin` as the execution runtime. The setting defaults to `false` and has no effect when another runtime is selected. Each image's resolved source determines the builder: `docker-local` can use the matching local Unix-socket Docker Engine, and `apple-local` can use Apple Containers. TCP and other remote Docker endpoints, registries, and archives are not build targets. Explicit builds against incompatible sources retain their warning; on-demand preparation imports normally. awman never switches stores implicitly.

Plain `awman ready` inspects a compatible local source store when normal readiness rules need to know whether an image exists. If no build is needed, an installed image that matches that source is reused without exporting it again. If the builder cannot be reached but that image is already installed and no build is pending, ready warns and continues with the installed image. A successful build forces a fresh import from the same store and reference, even if the acquisition cache contains an older image.

Apple Containers 0.12.0 reports an image index instead of a per-platform digest
in its inspection output; awman accepts that format for image freshness checks
and builds. Inspection failures name the image and the field that could not be
read. If `chat` still launches, its previously imported builtin
image remains usable: `ready` also checks the source store, which launch does
not need. Updating awman fixes older versions that rejected valid Apple 0.12.0
metadata even with `ready --build --no-cache`.

The project base image is built before dependent agent images in the same source store. Build and import have separate result rows; readiness requires both stages for each required image. A project image built only for a non-default agent's store is optional like that agent's image: if it fails, the row shows the failure, but `ready` still succeeds once every required image is ready. A build can succeed while import fails, in which case `awman ready` reports failure and can retry the import on a later run. `--no-cache` affects builds when a build is scheduled; by itself it does not schedule one, but it does re-acquire the image from its source. Use `--build` to request the usual forced builds.

If a required image stage fails or remains unfinished, `awman ready` reports `awman is not ready.` and exits with status 1, whether or not the builder is enabled. Pending build receipts are stored under the awman data home at `pending-builds/`; per-image build locks are under `locks/image-build/`. Two awman processes preparing the same store and reference take turns; the one that waits (a `chat` launch during a running `awman ready --build`, for example) prints `Waiting for another awman process to finish with <reference> in <store>…`. Source builds disable BuildKit provenance and SBOM attestations so the exported result has a verifiable single-platform image. Docker `npipe://` endpoints are unsupported for builtin image acquisition or building. A configured source is checked again when an image must be acquired; a cached builtin image is reused only when its identity matches the inspected source.

Docker builds require a local Docker CLI and daemon. If Docker Buildx is installed, its `default` builder must use the `docker` driver; other drivers fail with an actionable error. Apple builds require the Apple Containers CLI/service. The source store is used for building and importing, while agent sessions continue to run only in the builtin microVM.

Importing enforces safety limits: a total archive of up to 32 GiB, up to 8 GiB per layer after decompression, up to 256 layers, and at least 1 GiB of free disk space. Archive metadata and the selected image's layers are validated before import: digests must match, the image must be for the host's Linux platform, and selected images with path traversal, writes through symlinks, device nodes, unsafe hard links, or malformed whiteouts are rejected (`image archive … rejected: <reason>`). Only the selected image and requested tag reach the runtime importer; unselected images and unrelated tags are excluded, while selected config and layer bytes are preserved. Plain and gzip-wrapped tars are supported; zstd-, xz-, or bzip2-wrapped archives, zstd-compressed layers, and non-distributable/foreign layers are refused with a reason naming the problem and, for zstd, the gzip re-export that works instead. Launching a session uses the already-imported image and never re-reads its source; only a launch with the builder enabled and its agent image missing acquires one first, as described above.

### Configuration

`runtime.kind` is global; the `runtime.builtin` settings can be set in the global config and per repo. Repo values override global ones per field, and the `images` and `registries` maps are replaced as a whole when the repo sets them.

| Key | Default | Meaning |
|---|---|---|
| `runtime.builtin.stateDir` | `<AWMAN_HOME>/builtin` | Private builtin runtime state root for imported images, VM/session state, catalog and control sockets. The directory is process-wide: every builtin session in the awman process uses it, and a repo `runtime.builtin.stateDir` that resolves to a different directory is rejected at launch with an error naming both paths. Must be an absolute path without `..`, owned by you with mode `0700`, and contain no symlinks. Keep it short: control sockets live below it and paths are limited to about 100 bytes (`socket path … is longer than the … limit; configure a shorter builtin state directory`). `AWMAN_BUILTIN_STATE_DIR` overrides config for the process. |
| `runtime.builtin.vcpus` | `2` | Whole vCPUs per session VM, 1–255. Fractional values are rejected, never rounded. |
| `runtime.builtin.memory` | `4GiB` | Requested guest RAM, 128MiB or more. Allow additional host memory for VM overhead. |
| `runtime.builtin.rootDisk` | `30GiB` | Writable root disk per session VM, 256MiB or more: the space the agent has for everything outside its mounts (`/`, its home directory, installed packages). The disk file is sparse, so it only uses host space as the agent writes. |
| `runtime.builtin.mountQuota` | `30GiB` | How much the agent may add to each host mount, the workspace included, beyond what it already held; 1 or more. Writes past it fail with "no space left on device" in the guest. This keeps an agent from filling your disk through a mount. |
| `runtime.builtin.builder.enabled` | `false` | Let `ready`, and a launch whose agent image is missing, build in a `docker-local` or `apple-local` source store before importing. See [Building from a local image source](#building-from-a-local-image-source). |
| `runtime.builtin.imageSource` | unset | Default image source (see above). |
| `runtime.builtin.images` | `{}` | Per-image-tag sources. |
| `runtime.builtin.registries` | `{}` | Per-registry settings. |
| `runtime.builtin.network` | `{"mode":"public"}` | Guest network policy — see [Network](#network) below. |

Builtin VM settings are resolved per session from that session's own repo, in the TUI, the API and squad alike. A repo's `vcpus`, `memory`, `rootDisk`, `mountQuota`, `registries` and `network` override the corresponding global values for the sessions launched in that repo (`network` subject to the ceiling below), so two repos served by one awman process can run with different limits. `runtime.builtin.stateDir` is the one process-wide setting: a repo value that resolves to a different directory than the one the process uses refuses the launch with an error naming both paths; remove it from the repo config or set it to the process's path. `vcpus`, `memory`, `rootDisk` and `mountQuota` can be changed with `awman config set` (for example `awman config set runtime.builtin.vcpus 4 --global`). The object-valued keys are shown read-only by `awman config show`; edit them in the JSON file. The `runtime.builtin` settings are only used when `runtime.kind` is `builtin`, and an invalid one is an error naming the bad key.

The builtin runtime refuses startup if any of these Microsandbox override variables is set: `MSB_PATH`, `MSB_LIBKRUNFW_PATH`, `MSB_AGENTD_PATH`, `MSB_HOME`, `MSB_BACKEND`, `MSB_CONFIG_PATH`, `MSB_PROFILE`, `MSB_CACHE_DIR`, `MSB_SANDBOXES_DIR`, `MSB_VOLUMES_DIR`, `MSB_SNAPSHOTS_DIR`, `MSB_LOGS_DIR`, or `MSB_SECRETS_DIR`. Unset the named variable and retry.

### Network

Every session VM gets its own network stack running in the host-side worker process. Hostname allow rules are limited to TCP; UDP/QUIC is denied because the stack cannot inspect its hostname boundary for those protocols. Configure it under `runtime.builtin.network`, global and/or per repo:

```json
{
  "runtime": { "builtin": {
      "network": {
        "mode": "allowlist",
        "allow": ["api.anthropic.com", "*.github.com"],
        "hostPorts": [8765],
        "nameservers": ["1.1.1.1", "[2606:4700::1111]:53"],
        "trustHostCas": false
      }
  } }
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

**Layering.** The global `runtime.builtin.network` block is a ceiling a repo config cannot raise, since repo config is part of the checkout and may be untrusted: a repo may pick a stricter `mode`, a subset of the global `allow` list, and a subset of the global `hostPorts`, but never something wider — asking for more is a config error, not a silent downgrade. `nameservers` and `trustHostCas` are read from the global config only. The repo's block narrows the policy for every builtin launch in that repo, in the TUI, the API and squad.

**CAs.** By default the guest trusts only the image's own CA bundle. Set `trustHostCas: true` (global only) to also install the host's trusted root CAs in the guest at boot — useful behind a corporate TLS-inspecting proxy. awman never installs an interception CA of its own.

> Real-guest checks on Apple Silicon cover allowlist and denied-egress enforcement (DNS, direct IP, DNS-over-HTTPS, proxy bypass), `hostPorts` access, `none` mode, and concurrent VMs with different policies. Linux x86_64 KVM checks also prove authorized and denied host-port access in concurrent VMs and that `none` leaves no active egress interface or route. IPv6 denial is verified only on hosts that have IPv6 connectivity.

### Session lifecycle

- Each agent session, and each workflow setup/teardown environment, is its own microVM, removed when the session ends.
- Ctrl-C in the launching session sends the agent an interrupt and gives it 10 seconds to exit before the VM is killed. In `awman attach`, Ctrl-C detaches only that client.
- **The awman process that launched a session must keep running.** If it exits, the session's VM stops. Detaching and reattaching works while that process is alive: another client, including a newer compatible awman process, can attach to the same live session and see the same output; detaching only ends that one client's connection, and the guest keeps running. Reattaching after the launching process has exited is not supported. Checkpoint restore is not supported.
- **A client is never left believing it saw the whole session when it didn't.** If a client reading attached output falls too far behind the live stream, awman ends that connection with an explicit failure — never a later clean exit that would make missed output look like it was delivered. Reconnect (or restart the agent) after seeing this. An attach starts at the current output; it does not replay what was written before the connection was accepted.
- An explicit stop of a builtin session keeps an inspectable stopped instance (repeating the stop, or stopping one that is already gone, is safe); removing it (`awman clean`, or an explicit remove) deletes it and its disk. Awman refuses to act on an instance that was replaced by another with the same name.
- `awman agents status` lists builtin sessions. Workflows and squad find their sessions by name, as they do on Docker.
- `awman agents detail <id>` includes the builtin VM's configured vCPUs, memory in MiB, and network mode.
- Live CPU and memory figures (the TUI's container title bar and minimized bars, and `awman agents status`) are measured from the VM itself: CPU is the guest's vCPU time over wall time on the same per-core scale as `docker stats` (100% is one fully busy vCPU, so a 2-vCPU VM tops out at 200%), and memory is what the guest is using. The runtime publishes a new sample once a second, so a one-shot `awman agents status` may take about a second per running session to report CPU.
- If you replace the awman binary while a session is running, new operations report `awman was replaced on disk while this session was running; restart awman to use the builtin runtime`. An awman with a different internal protocol version refuses to adopt sandboxes created by another version (`builtin runtime worker protocol mismatch … stop running agents started by an older awman and retry`): stop those agents with the awman that started them first. A state directory created by an older awman is handled differently: it is upgraded in place (see below).

**Upgrading an older state directory.** When the state directory was created by the awman release this awman can upgrade from, it is upgraded in place the first time this awman opens the builtin runtime. Only exact protocol versions are accepted; this release upgrades from `0.7.2/18/awman-1` only, and any other older protocol is refused (see *Catalogs that cannot be upgraded* below). There is nothing to move or delete, and no prompt. The previous catalog is first copied to `db.pre-<protocol>` beside the `db` directory (for example `~/.awman/builtin/db.pre-0.7.2_18_awman-1`), together with a manifest, `awman-catalog-backup.json`, that lets awman check the copy later. Your imported images and the download cache are kept. Leftover sandboxes of the older awman (they have all stopped) are removed, records and files. The CLI prints one line on stderr; the TUI shows the same line in its status bar until you press a key:

```
upgraded builtin state <state dir> from <old protocol> to <new protocol>; previous catalog saved in <backup>
```

If the older awman is still running agents from that directory, or is launching one, or importing or removing an image, the upgrade is refused and nothing is changed: `the builtin state directory <dir> was created by an older awman (protocol <old>) that is still running or launching agents, or importing or removing an image, from it; quit that older awman, then retry so this awman can upgrade the directory in place`. Quit that awman and retry. While the upgrade runs, an older awman that is still open cannot start any of those: a launch or image removal fails at once, and an import waits until the upgrade has finished.

If an upgrade is interrupted (a crash, or a failure that could not be undone), the next run resumes it from the backup automatically, after checking the backup against its manifest. If awman cannot prove that is safe (for example the backup has no manifest, or its files do not match it, or `db` was never migrated), it refuses and changes nothing: `cannot upgrade the builtin state directory <dir> from protocol <old>: a catalog backup from an earlier, interrupted upgrade already exists at <backup>, and awman cannot prove it is safe to restore automatically. If db/ still holds your catalog, move the backup aside and retry; if db/ is missing or empty, restore it from the backup first`. To restore `db` from the backup, follow steps 1–3 below, then retry. Do not delete the backup until `db` holds a good catalog.

**Rolling back.** Use the backup with the older awman binary. There is no downgrade path otherwise. With the state directory in place of `~/.awman/builtin`:

1. Quit every awman (old and new) using the state directory.
2. Keep the upgraded catalog aside and copy (do not move) the backup into place:
   ```
   cd ~/.awman/builtin
   mv db db.after-upgrade
   cp -Rp db.pre-0.7.2_18_awman-1 db
   rm db/awman-catalog-backup.json
   ```
   The backup's database already contains its write-ahead log, so there are no `-wal` or `-shm` files to bring along.
3. Write the older protocol back into the marker, exactly and without a trailing newline (the older awman refuses the catalog otherwise):
   ```
   printf '%s' '0.7.2/18/awman-1' > awman-runtime-version
   ```
4. Run the older awman binary. Its catalog still lists the stopped sandboxes the upgrade removed, but their files are gone: do not resume them, remove them with the older awman.

Once you are happy with the upgrade, delete the `db.pre-*` backup (see [Cleaning up](13-cleaning-up.md)).

**Catalogs that cannot be upgraded.** A catalog with no recorded protocol version, one created by a newer awman, or one created by an older awman whose protocol this release does not list, is never modified, and no downgrade is attempted. Every builtin command then reports:

```
builtin runtime worker protocol mismatch: the builtin state directory <dir> was created with protocol <found>, but this awman uses <expected>. Quit any older awman still running agents from it, then move or delete that directory (or set runtime.builtin.stateDir to a new one) and run `awman ready` to re-import images
```

For a newer catalog, use the newer awman. Otherwise quit any awman still using the directory, move it aside or delete it (or point `runtime.builtin.stateDir` at a new one), and run `awman ready` to re-import your images.

### Parity limits versus Docker and Apple Containers

| | Docker / Apple Containers | Builtin |
|---|---|---|
| Image creation | `ready` builds from Dockerfiles | `ready` imports from a configured source; with `runtime.builtin.builder.enabled` it can first build in a local Docker or Apple Containers store |
| Fractional CPU | Supported | Rejected (`cannot honour …`): whole vCPUs only |
| Resource limit enforcement | Container-level limits | Whole vCPUs and a requested RAM allocation per VM. Guest-visible memory depends on the platform boot layout and excludes kernel reservations. Guest allocation exhaustion uses the guest's OOM killer; budget host memory for VM overhead and concurrent sessions too. |
| `--allow-docker` | Mounts the host Docker socket (Docker only) | **Rejected.** No socket is ever mounted and there is no host daemon bridge |
| Reattach after the launching awman exits | Docker: yes. Apple: no | No |
| Host `localhost` / MCP services | Per the container network | Inside the VM, `127.0.0.1` is the VM itself; a host loopback service is reachable only if its port is listed in `runtime.builtin.network.hostPorts` (see [Network](#network)) |
| Egress control | None built in (Docker); per-container network (Apple) | `runtime.builtin.network.mode`: `public` (default), `allowlist`, or `none`. HTTPS checks visible SNI plus its exact DNS/IP binding; encrypted HTTP authority and ECH inner names are not inspected. Hostname rules deny UDP/QUIC |
| Named image `USER` (like `awman` in the templates) | Handled by the container engine | Resolved to a numeric id from the imported image's `/etc/passwd` and `/etc/group`; an unresolvable user is refused. Agents never run as root |
| Image `HOME` | From the image | From the image's `HOME`, or else the image user's home directory in `/etc/passwd` (as Docker does); an image with neither is refused |
| Exit status of an agent killed by a signal | `128 + signal` (for example `137` after the OOM killer) | `-1`: the embedded guest agent reports that the process was killed by a signal but not which one; awman's own exit status is then `1` |
| Overlays (`dir`, `skill`, `env`, `context`), agent settings and system prompts | Bind mounts / env | Same overlay semantics. Directories are directory mounts and single files are single-file mounts (the parent directory is never mounted). Sockets can't be mounted as overlays |

### Troubleshooting

| Message | Cause and fix |
|---|---|
| `the builtin runtime is unavailable: this build of awman does not include the builtin-runtime feature` | This awman build lacks the runtime. Use an asset that includes it or build from source (see above), or select another runtime. |
| `the experimental builtin runtime is only available on macOS on Apple Silicon …` | This release supports Apple Silicon Macs and Linux x86_64 with KVM. On other hosts, select a supported runtime such as `docker`. |
| `runtime.kind: expected one of docker, apple, sbx, builtin` | The value is not one of the recognized runtime names (an earlier spelling such as `builtin-experimental` counts). Run `awman config set runtime.kind builtin`. |
| `embedded worker startup failed; verify KVM access or the macOS com.apple.security.hypervisor entitlement …` | On Linux, check read/write access to `/dev/kvm` and virtualization support. On macOS, the binary may lack its entitlement or have been modified after signing: run `make sign-macos BIN=$(command -v awman)` or reinstall it. |
| `"awman-macos-arm64" cannot be opened because the developer cannot be verified` | The file is quarantined (downloaded with a browser). Verify its checksum, then `xattr -d com.apple.quarantine <file>` — see [The macOS binary, Gatekeeper and permissions](#the-macos-binary-gatekeeper-and-permissions). |
| `builtin image <tag> has no absolute effective HOME` | The image sets no `HOME` and its user has no absolute home in `/etc/passwd`. Add `ENV HOME=/path` to the Dockerfile. |
| `the builtin runtime refuses to start while <variable> is set …` | An environment override would replace the builtin runtime. Unset the variable named in the error and retry. |
| `builtin state directory requires private ownership (0700) and trusted parents` | The state directory must be owned by you with mode `0700`; its parent directories must not be writable by other users. New awman directories are created privately. For an existing personal `~/.awman`, check its ownership and use `chmod 700 ~/.awman` if it is writable by your group. Choose a private state location instead of changing a deliberately shared directory. |
| `no image source is configured for '<tag>'` | In an interactive CLI or TUI run, choose a source in the `ready` setup prompt. In non-interactive runs, set `runtime.builtin.imageSource` for the default source or `runtime.builtin.images.<tag>` for that image, then run `awman ready` (see [Interactive image-source setup](#interactive-image-source-setup)). If building is wanted, also set `runtime.builtin.builder.enabled` to `true`; source selection alone only imports. |
| ``builtin image <image> is not imported; run `awman ready` to import it``, or ``image '<tag>' is not in the builtin image cache (…); run `awman ready` to import it`` | Run `awman ready`. |
| ``a builtin sandbox for agent '<name>' already exists; stop or remove it (`awman clean`) or choose another name`` | A session with that name is still present; stop it, run `awman clean`, or pick another name. |
| `the <builder> image builder is unavailable: <detail>` | `runtime.builtin.builder.enabled` selected Docker or Apple Containers for this image's source, but it cannot be used (CLI missing, daemon or service stopped). Start it to prepare updated images. A session can continue with an installed image and a stale-image warning; explicit `ready --build` fails. Disable the builder to launch installed images without source freshness probes. |
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

### Reading `ready` failures

The CLI and TUI show a readable `ready` summary. Failed and warning rows show their reason once below the row; the summary's recovery hint, when present, appears once as `Tip:`. JSON and API output keep the status and diagnostic in their structured fields; they do not start the interactive source setup flow.

For a `docker-local` source, the diagnosis depends on the endpoint and the failure:

| Message | What it means |
|---|---|
| ``Docker is not running at <endpoint>. Start Docker, then run `awman ready` again.`` | The selected Unix socket returned a missing or refused connection. Start Docker and retry. |
| ``No Docker daemon at the default socket <endpoint>. Start Docker, or set the source's `host` (or DOCKER_HOST) if Docker uses another socket/context, then run `awman ready` again.`` | The platform-default Unix socket is absent or refusing connections. Start Docker, or select the socket/context explicitly in `host` or `DOCKER_HOST`. |
| ``Cannot access the Docker socket at <endpoint>. Check socket permissions and your Docker connection context (`host` / DOCKER_HOST), then run `awman ready` again.`` | The socket denied access. Check its permissions and the selected Docker connection context. |
| `Docker Engine at <endpoint> is not reachable: <cause>` | A TCP or other remote endpoint failed. This is reported as an unreachable endpoint with its connection cause; it is not described as Docker being stopped locally. Transient connection failures may be retried a bounded number of times. |
| ``The Docker endpoint <endpoint> is not a usable Docker socket. Check the source's `host` / DOCKER_HOST, then run `awman ready` again.`` | The selected Unix path is not a usable socket. Correct `host` or `DOCKER_HOST`. |

These messages apply to configured sources. A stopped or inaccessible source does not cause `ready` to ask for another source or silently switch to a different store. Fix the endpoint/service and rerun `ready`.

---

## Docker Sandboxes (experimental)

The `sbx` runtime is disabled in v0.14 because current Docker Sandboxes kits are
incompatible with awman's kit definitions. Setting `runtime.kind` to `sbx`
returns an explicit error before starting an agent. Select `docker`, `apple`,
or `builtin` to run agents. Existing sandbox state is not deleted.

See [issue #86](https://github.com/prettysmartdev/awman/issues/86) for the
compatibility work and the plan to restore this runtime.

### Known limitations

All SBX agent operations are unavailable in v0.14.

---

## Native modes, profiles and automation by runtime

The native-agent features — `--plan`, `--read-only-workspace`, `--settings-profile`, `--agent-session` and `--automation` — behave the same on Docker, Apple Containers and the builtin runtime, on the host platforms each is released for. awman refuses, before building anything, a combination a runtime cannot honor; it never starts the agent with weaker guarantees instead.

| Feature | Docker, Apple Containers, builtin |
|---|---|
| `--plan` | All eight agents |
| `--read-only-workspace` | Enforced by the workspace mount where the runtime can; otherwise refused at preflight |
| `--settings-profile`, `--agent-session` | Supported; only the private per-execution profile copy is mounted |
| `--automation` | Supported; a private channel directory is mounted at `/awman/automation` for native lifecycle records |

Things that depend on the runtime and are reported honestly rather than assumed:

- **Image versus authentication readiness.** `awman ready --prepare-only` prepares the images for the selected runtime without checking authentication (see [`ready --prepare-only`](03-agent-sessions.md#preparing-images-without-authentication-ready---prepare-only)). On Docker and Apple Containers it builds the images; on the builtin runtime it imports them from the configured image source (`--build` re-acquires, `--no-cache` forces a fresh import). To refresh Claude or Codex with a newer CLI, rebuild their images with `awman ready --prepare-only --build --agent claude` (or `codex`); no awman release or version approval is needed, and an unreadable CLI version is only a warning.
- **Discovering what a runtime offers.** `GET /v1/status` lists `read_only_workspace` only when the daemon's runtime can enforce it, and `GET /v1/worker/sessions/{id}/agent-capabilities` names the runtime each declaration applies to, without starting an agent. Declarations are not guarantees: the per-agent status tier is established from the running agent.
- **Native status versus OSC.** Full native lifecycle status needs the provider to emit its own lifecycle records into the automation channel. Where that is not available, the OSC 7501 reports the agent already sends supply `best_effort` status; `agents.programStatus.enabled=false` turns that off and with it OSC-based admission. Consumers see which tier applied in every turn result and command result.
- **First-byte grace versus automation startup.** Ordinary launches use a first-output grace period; automation launches instead use `agents.automation.startupTimeout`, and a slow runtime start (for example the Apple Containers start delay) does not trigger the ordinary grace-period kill.
- **Credentials.** Replacing a credential inside a running agent or restarting an agent to apply one is not currently offered on any runtime.

## Switching runtimes

Change `runtime.kind` to select a supported runtime. Docker and Apple
Containers use their own image stores; builtin uses a private image and VM
store. Changing the selection does not delete another runtime's state.

```sh
awman config set runtime.kind builtin
awman ready
awman chat --agent claude

awman config set runtime.kind docker
awman ready
awman chat --agent claude
```

`awman ready` prepares the selected runtime. Selecting `sbx` in v0.14 fails
with a disabled-runtime error; choose a supported runtime to continue.

---

[← GitHub Integration](10-github-integration.md) · [Next: squad →](12-squad.md)
