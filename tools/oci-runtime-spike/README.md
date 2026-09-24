# Builtin OCI runtime spike: Microsandbox versus smolvm

Research date: 2026-09-24. This is a spike, not a work item or a backend implementation.

Follow-up decision: smolvm is discarded. The historical comparison below is retained as evidence. See [the Microsandbox strict-embedding spike](strict-embed/README.md) for the newer feasibility results and bundled-versus-strict effort comparison.

SQLite follow-up: [the dependency-resolution spike](sqlite-resolution/README.md) resolves Microsandbox's conflict with a one-line, already-merged SQLx backport, without downgrading awman's rusqlite. The actual awman library and strict msb runtime now link together on Linux ARM64 and pass database checks. The unpatched failures below remain historical evidence, not an unresolved design decision.

## Recommendation

**Prefer Microsandbox as the next integration candidate, initially as an awman-managed, bundled worker. Do not declare either candidate fully compatible yet.** The corrected Apple Silicon run passed all nine Microsandbox image assertions and all 32 mount/config assertions, including single-file sharing and live atomic refresh. Smolvm has a useful Rust embedding interface, but the tested release has concrete incompatibilities: directory-only host mounts, rejection of pure OCI-layout archives in its image-flattening path, incorrect opaque-whiteout handling, and lost image runtime defaults in the tested local-archive launch path. Full awman integration, Linux execution and real source-store interoperability remain unverified.

**Both libraries fail dependency resolution when directly combined with awman's current SQLite dependency.** Consequently, neither is a drop-in Cargo dependency today. A bundled subprocess avoids this conflict without changing awman's database stack. A one-file download that extracts private runtime assets is feasible; a genuinely asset-free, purely Rust awman executable is not demonstrated by either project.

The development host here is Linux ARM64 without `/dev/kvm`, Docker, Apple Containers, or a Mac. Only host-side experiments ran here. Two user-run Apple Silicon experiments subsequently demonstrated guest execution on both runtimes. The first run's Microsandbox failure was caused by the harness's overly long state path and was resolved in the second run. No authenticated agent session ran. Source inspection and synthetic smoke tests are not a security certification.

## Corrected Apple Silicon results

The second user-supplied `results.tar.gz` has SHA256 `ec3690843855ee1b34e80312fec83f1e9f5d3b6453a16f8fbb1a714c47b7eb49`. Selected raw outputs are retained under `results/mac-arm64/`. The platform is macOS 26.6.2 ARM64, with the same pinned runtime releases.

| Check | Microsandbox | smolvm |
| --- | --- | --- |
| Run image ENTRYPOINT/CMD without an explicit command | PASS; entrypoint marker and image checker executed | FAIL; foreground CLI rejects command-less invocation |
| Image USER / primary GID | PASS / PASS | FAIL / FAIL in explicit-command Docker-archive launch |
| Image WORKDIR / HOME / ENV | PASS / PASS / PASS | FAIL / FAIL / FAIL in that same launch |
| Ordinary file whiteout | PASS | PASS |
| Opaque-directory whiteout | PASS | FAIL; hidden lower-layer child survives |
| Upper-layer file / executable mode | PASS / PASS | PASS / PASS |
| Exit-code propagation and stdout/stderr separation | PASS | PASS |
| Mount/config suite | 32 PASS; completion marker present | 27 PASS; completion marker present; file-dependent cases UNSUPPORTED |

Microsandbox's mount pass includes file ro/rw, Claude's config file, file/env prompt delivery, directory-based settings strategies, nested ro mounts, context scopes, skill directories, atomic credential refresh, basic writeback and the synthetic remount/escape checks. This provides concrete evidence for the primitives awman needs; it does not prove actual agent/frontend integration or comprehensive isolation.

Smolvm's image checker exited 6 because six independently reported assertions failed: user, group, workdir, HOME, image environment variable, and opaque whiteout. The default-command rejection is a separate failure. **Do not generalize the five metadata failures to all registry or persistent-machine modes**: this run used a local Docker-save archive and an explicit command, which is itself an important required import/launch path. Source inspection identifies a likely explanation: the foreground path sets `image_info = None` for packed/local sources, then passes that missing metadata into runtime-default resolution (`src/cli/machine.rs`, around lines 1867 and 1977 in the pinned checkout). This source-level explanation has not been validated with a patched build.

The short state path fixed all observed Microsandbox startup failures; no re-signing, extra packages or elevated privileges were needed in this run. Smolvm still emitted its `resize2fs` fallback warnings. Neither optional image-store export ran. No further rerun of the same suite is necessary for this comparison; subsequent testing should target the untested acceptance gates rather than repeat these checks.

## First Apple Silicon results

The user supplied `results.tar.gz`, SHA256 `0b962fac1342ea1c67586a0a4571c4120cf2c897805bcda29d2ece5a69ed0691`, from macOS 26.6.2 ARM64. This updates, rather than replaces, the initial Linux-only observations below.

- Both release signatures verified; their recorded entitlements include `com.apple.security.hypervisor`. Microsandbox doctor, public registry pull, Docker/OCI fixture imports and OCI export passed.
- All Microsandbox guest launches failed before boot because the harness put `MSB_HOME` under macOS's long per-user temporary path. Derived socket paths were 139 or 146 bytes; the runtime requires less than 104. This is a harness setup failure, **not evidence of broken guest execution**. The runner now allocates a short, private `mktemp` directory under `/tmp`. Awman's eventual backend must also budget Unix socket path lengths.
- Smolvm executed guest commands, propagated exit status 37, and separated stdout/stderr successfully. Its directory suite recorded **27 PASS checks**, including nested read-only sharing, skills, context scopes, directory-based config strategies, host writeback, atomic credential refresh, and the synthetic escape/remount checks. File-based cases remain explicitly unsupported. These are bounded checks, not proof against all sandbox escapes.
- Smolvm's default-command probe did **not** reach the image checker: the CLI rejected an image-only foreground invocation with `no command specified`, despite the image's ENTRYPOINT/CMD. The pinned `src/cli/machine.rs` guard confirms this behavior. The runner retains that default-command compatibility check and now separately invokes `/compat/check-image` explicitly to test USER/HOME/WORKDIR/ENV and whiteouts. The new checker reports every assertion individually rather than stopping at the first failure.
- Smolvm warned that host `resize2fs` was missing when requesting disks smaller than its 20 GiB storage / 10 GiB overlay templates, but guest execution still succeeded. This is not a demonstrated mandatory external dependency; actual disk-sizing behavior after fallback needs measurement. Do not install e2fsprogs merely to hide the warning in the next run.
- Neither optional source-store export was selected, so actual Apple-store and Docker-daemon interoperability remain untested.

The subsequent corrected run is recorded above. The revised runner checks guest-suite completion explicitly and suppresses macOS AppleDouble metadata when packaging results. The first run alone did not establish Microsandbox mount behavior or smolvm image-default/whiteout correctness inside a VM.

## Run the Mac checks

From the awman checkout on an Apple Silicon Mac:

```bash
bash tools/oci-runtime-spike/mac-checks.sh
```

Prerequisites: an existing Rust/Cargo toolchain and its native linker (normally Xcode Command Line Tools), internet access, and normal macOS command-line utilities. The script does not install Rust. Rust is needed only to build the synthetic image fixture, not as a proposed end-user runtime dependency. Allow several minutes and spare scratch disk space. VMs run sequentially, with one vCPU and 512 MiB each; smolvm requests 1 GiB storage and overlay disks, but may warn and take a fallback path without host resizing tools. Actual disk consumption is not yet verified.

The runner downloads checksum-pinned ARM64 releases, verifies their signatures without modifying them, pulls public Alpine, creates Docker/OCI fixtures without an image builder, and tests defaults, exit codes, separate output streams, and mount/config contracts. It uses a new temporary directory, synthetic credentials, and isolated runtime stores. It does not use sudo, run real agents, mount your home, or read real agent credentials. Downloads, Cargo cache, images, and VM state remain in the printed directory. No shell profile or global executable is changed. Guest command timeouts are configured; startup itself may still hang in an upstream runtime. Interrupt with Ctrl-C if necessary and retain the logs.

To also test export from stores you already have, explicitly select existing, non-sensitive images:

```bash
APPLE_IMAGE='your-existing-image:tag' DOCKER_IMAGE='your-existing-image:tag' \
  bash tools/oci-runtime-spike/mac-checks.sh
```

Either variable can be omitted. These optional checks invoke your installed `container image save` / `docker image save`, using your existing source-store connection, and import into the isolated Microsandbox store. They do not build, run, delete, or retag the source images. Docker's selected context/host may be remote. **This validates export/import interoperability, not a completed CLI-free store adapter.** Large images require more disk and time. The selected source archives stay local and are excluded from the share bundle.

At completion, review and share the printed `results.tar.gz`. It contains logs and summaries, not image archives, VM disks, Cargo caches, or config directories. Logs can still contain your username, paths, selected image names, or service errors; review them first. If setup fails early, share the printed logs directory instead. Failed checks are recorded rather than hiding later results; the runner finishing does not mean all checks passed. Smolvm's mount suite deliberately marks file-dependent cases `UNSUPPORTED`, not `PASS`.

The revised Mac runner was executed by the user; its results are recorded above. It cannot run on this Linux development host. It does not yet automate PTY resize, full lifecycle/reattach, network policy, resource enforcement, or actual awman frontend integration; those remain acceptance gates below.

## Pinned inputs and evidence

| Component | Examined version |
| --- | --- |
| awman | `8765a9b95db1f0b2444ee999e3b5908ac59b78ee` plus the existing working tree; no application source changed |
| Microsandbox | `v0.7.2`, commit `60d4dc8a436fb9365491567ec21d073e924e3c6d` |
| smolvm | `v1.18.1`, commit `c890ec3dae904ca9d47e64b084a23cfbf2230849` |
| Fixture tool | Rust 1.94.0; standalone lockfile under `fixture/` |
| Base | `docker.io/library/alpine:3.22`, ARM64 manifest reported as `sha256:2e1a7aa4cbc4e9e5222bb4c24a839aa1a6170ea5492d644777ce7b178824e44f` |

Release archives were downloaded and SHA256-checked. Linux ARM64 archive hashes: Microsandbox `d4de7814147b835a4b99e51e236c8b333340091e5727a0b45d7eb847d56b022a`; smolvm `a4a971e1c08dec027f2ae45b9f8cde50b91cb87f6ce8d940d680af1dd7597d9b`. Mac hashes are pinned in the runner. The rerun pulls the Alpine tag and may receive a newer base; runtime versions remain pinned.

Small raw outputs are under `results/linux-arm64/` and `results/mac-arm64/`. Exit codes in the Linux `results.tsv` are observations, not verdicts: for example, successfully extracting a supposedly deleted file is a compatibility failure. Fixture images and upstream checkouts are intentionally not committed.

### Initial Linux-host experiments

| Check | Microsandbox | smolvm |
| --- | --- | --- |
| Release executable starts | PASS, 0.7.2 | PASS, 1.18.1 |
| Pull public registry without Docker | PASS, Alpine ARM64 | NOT RUN: its normal OCI execution path requires a VM |
| Import Docker-save fixture | PASS | Config accepts archive; bundled `crane export` processes it; full guest import BLOCKED |
| Import pure OCI-layout fixture | PASS | FAIL in the bundled guest helper, executed directly on Linux ARM64: `file manifest.json not found in tar` |
| Preserve USER/WORKDIR/ENV/ENTRYPOINT/CMD/labels/layer count | PASS in stored Microsandbox image metadata, not guest execution | Guest metadata recovery is implemented, not execution-tested |
| Export imported image as OCI | PASS; guest behavior not implied | Not a comparable host-side import/export check |
| Single-file host mount | Dedicated implementation found; VM test BLOCKED | FAIL at CLI validation: `source path on host must be a directory (virtiofs limitation)` |
| Directory host mount | Source support; VM test BLOCKED | Configuration creation PASS; VM behavior BLOCKED |
| Ordinary whiteout in flattened Docker fixture | Guest application BLOCKED | PASS in bundled helper output: `compat/remove-me` absent |
| Opaque-directory whiteout in same fixture | Guest application BLOCKED | FAIL in bundled helper output: `compat/opaque/old` survives |
| Fractional `--cpus 0.5` | REJECTED by parser | REJECTED by parser |
| Direct library dependency with awman rusqlite 0.40 | FAIL at Cargo resolution | FAIL at Cargo resolution |
| VM execution on this host | BLOCKED, no `/dev/kvm` | BLOCKED, no `/dev/kvm` |

Smolvm ships `crane` 0.19.0 in the examined guest rootfs. Running that exact helper on the host exercises its archive transformation without booting a VM. This is not evidence that an entire smolvm VM ran. Its output retains the lower-layer opaque-directory child but removes the ordinary whiteout target. The fixture also checks that upper-layer `compat/opaque/new` survives. Source confirms the normal archive path invokes this helper, then recovers config through Docker `manifest.json`. See [smolvm archive processing](https://github.com/smol-machines/smolvm/blob/c890ec3dae904ca9d47e64b084a23cfbf2230849/crates/smolvm-agent/src/storage.rs#L1411). Upstream go-containerregistry now contains explicit opaque-directory handling; updating or replacing the bundled flattener is a plausible fix, not one tested here. See [upstream extraction implementation](https://github.com/google/go-containerregistry/blob/main/pkg/v1/mutate/mutate.go).

## Architecture and packaging comparison

| Aspect | Microsandbox | smolvm |
| --- | --- | --- |
| Isolation | Linux guest behind hardware virtualization; local mode avoids hosted service | Linux guest behind libkrun hardware virtualization; local runtime |
| Required hosts | Apple Silicon execution/image/mount probes passed; Linux ARM64/x86_64 execution pending | Apple Silicon execution/directory probes passed, image contract failed; Linux execution pending |
| Host prerequisites | macOS hypervisor entitlement; Linux accessible KVM, firmware, applicable system libraries | macOS hypervisor entitlement; Linux accessible KVM, libkrun/firmware assets |
| Rust integration | Rust SDK/runtime crates; SDK local path still orchestrates runtime processes | Rust library, embedded APIs, explicit self-reexec boot entrypoint |
| Single-file distribution | SDK has embed-binaries support that packages/extracts executable plus firmware; not a helper-free runtime | Reexec allows awman to be the worker executable, but libkrun, kernel/firmware and guest assets remain |
| Examined Linux ARM64 package | About 30 MiB compressed / 61 MiB extracted; msb and firmware | About 41 MiB compressed / 115 MiB extracted; wrapper, binary, libraries, guest rootfs/templates, additional helper |
| External Linux library finding | `ldd` also resolves `libcap-ng.so.0`, not present in release archive: must bundle/static-link or declare a dependency | Examined main binary/libkrun resolve normal libc/libgcc/libm; guest `crane` and tools are bundled, not Rust-only |
| Host file sharing | File and directory paths, read-only flags, UID/GID presentation controls | Live directory sharing; individual files rejected; staged copy/sync is explicitly not live sharing |
| Image model | Native OCI image management usable without booting a guest | Registry/archive processing crosses into guest; richer machine/pack/snapshot model |
| Main attraction for awman | Closest mount/image contract to Docker/Apple | Embedding/reexec architecture, persistent machine management, snapshots/packs |
| Main integration cost | Database graph, worker packaging, full awman adapter and hardware validation | Same baseline plus file sharing, archive-format normalization and whiteout repair |

Both necessarily include a Linux kernel and nontrivial virtualization machinery. “No external runtime install” is realistic; “no operating-system prerequisites or auxiliary bytes” is not. On Mac, do not assume an embedded worker inherits valid entitlements after copying/re-signing: test the exact distributed artifact. Neither runtime makes an ordinary cloud/container host with no KVM capable of running its VMs.

Microsandbox's embedding logic is visible in [SDK build.rs](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/sdk/rust/build.rs). Smolvm explicitly exposes self-reexec boot support in [its Rust library](https://github.com/smol-machines/smolvm/blob/c890ec3dae904ca9d47e64b084a23cfbf2230849/src/lib.rs#L90); its [platform notes](https://github.com/smol-machines/smolvm/blob/c890ec3dae904ca9d47e64b084a23cfbf2230849/README.md#L317) document signing requirements. These capabilities make single-awman-entrypoint designs plausible, not already integrated.

### Direct Rust linking blocker

Awman uses `rusqlite 0.40`, requiring `libsqlite3-sys ^0.38`. Microsandbox's local SDK includes `sqlx-sqlite 0.9`, which requires `libsqlite3-sys >=0.30.1, <0.38.0`. Smolvm uses `rusqlite 0.32`, requiring the 0.30 native binding. Cargo rejects multiple incompatible packages declaring `links = "sqlite3"`, even with Microsandbox default features disabled and only `local,net` enabled. Both minimal probes failed before compilation; no full linked binary was produced.

Reproduce with `bash link-probe.sh msb /path/to/microsandbox /new/probe-directory` or substitute `smolvm`. These probes intentionally fail for the pinned versions. Options: separate bundled worker now; upstream dependency alignment; or extracting a narrower runtime dependency boundary that does not carry the database. Do not silently downgrade awman's SQLite stack merely to force a spike to compile.

## Required image sources

Image discovery/import should be independent of the execution backend. A bare `my-agent:latest` is ambiguous between a registry and a daemon's private store; require a source selector and resolve to content identity plus native architecture.

| Source | Recommended adapter | Candidate impact / outstanding check |
| --- | --- | --- |
| Remote or localhost OCI registry | Native registry client, platform selection, digest verification and explicit auth | Microsandbox public pull passed; private auth, credential helpers, localhost registry reachability and offline cache still need tests. Smolvm needs to make host-local registry/auth reachable from its guest or use a host importer. |
| Local Docker Engine / Docker Desktop | Docker Engine image-export API over selected Unix socket, importing an image archive | No Docker daemon needed for execution after import. Never read Docker's private overlay/containerd directories directly. Fixture models save format, not a live daemon round trip. |
| Remote Docker Engine | Same image-export API over authenticated TLS or selected SSH transport | Distinct from a remote registry. Honor explicit contexts/host and daemon platform; API negotiation, interrupted transfers and credential handling untested. A Rust client/SSH transport can avoid requiring docker/ssh executables. |
| Apple Containers local store | First validate `container image save`, then implement a versioned importer bridge if CLI-free import is mandatory | Neither candidate directly understands Apple's private store. A bundled Swift/native export bridge is a possible approach requiring its own spike; directly scraping live service databases is not recommended. OCI exports fit Microsandbox; smolvm needs OCI-to-Docker normalization or an import fix. |
| Previously exported archive | Import Docker-save and OCI layout with config, layers, platform and digests retained | Microsandbox handles both tested fixtures. Smolvm's archive path is Docker-layout dependent despite broad OCI wording. |

Docker documents [Engine image export](https://docs.docker.com/reference/api/engine/version/v1.51/#tag/Image/operation/ImageGet); Apple documents [image save](https://github.com/apple/container/blob/main/docs/command-reference.md#container-image-save). Exporting from an existing engine naturally requires access to that engine/store; that must not become a dependency for running an already imported sandbox. **Strict CLI-free Apple-store access is unresolved for both candidates, not a feature Microsandbox automatically supplies.** The Mac runner collects the actual Apple archive layout rather than assuming it.

Existing awman Dockerfiles can continue to produce the images externally; the new runtime consumes the outputs. Native Linux ARM64 images are required on Apple Silicon and native Linux amd64/arm64 on their respective Linux hosts. Do not promise arbitrary cross-architecture images, Rosetta installation, Docker socket mounts, or every Docker runtime extension as part of OCI compatibility.

## Full awman compatibility inventory

This inventory maps the existing contract; it is not a claim that every row was exercised. Reuse `ResolvedContainerOptions`, the agent matrix, overlay resolution, config sanitization and credential refresh. Do not reuse the SBX launch/config model.

| Existing contract / code | Microsandbox assessment | smolvm assessment | Remaining validation |
| --- | --- | --- | --- |
| Directory overlay, ro/rw; `src/data/config/overlays.rs` | Matching primitives in source | Directory primitives; reject staged mode for parity | Live read/write, non-root UID, host observation, atomic rename, read-only enforcement |
| File paths accepted by resolved overlays; `src/engine/container/options.rs` | Dedicated singlefilefs | **Blocked**: individual file sources rejected | File identity after host rename, rw writeback, sibling exclusion |
| Skill overlays, named/path resolution | Reuse awman resolver, mount directories | Same for directory-backed resolution | Multiple skills, overlapping destinations, permissions |
| Context overlays: global/repo/workflow, ro/rw | Matching primitives | Matching directory primitives | Mount each resolved scope and honor selected permission |
| Env overlays: inherited and literal | Supported injection; choose private IPC/config rather than secrets in argv | Supported env and secret references | Exact values, unset/empty values, precedence; no secrets in logs/metadata/process args |
| Direct settings: Codex, OpenCode, Gemini, Crush, Cline; `src/engine/agent/agent_matrix.rs` | Live directory mounts appear suitable | Directory mounts appear suitable | Correct image HOME, actual non-root access, refresh/writeback |
| No settings mount: Maki, Copilot | No special obstacle | No special obstacle | No unintended host config exposure |
| Claude strategy; `src/engine/overlay/agent_settings.rs` | Can express staged `.claude` directory plus `.claude.json` file | **Blocked** by `.claude.json` single-file sharing | Sanitized settings/credentials, atomic access-token refresh without refresh-token leakage |
| Antigravity strategy | Existing awman staging plus directory sharing | Directory sharing plausible | Preserve existing keychain/staging logic, never hand host keychain to guest |
| Prompt Append/file, EnvFile | File mounts available in source | **Blocked** by single-file paths unless genuine file sharing added | File bytes, flags/env paths and image user's access |
| Prompt AppendInline, Replace | Reuse resolved argv | Reuse resolved argv | Exact quoting, newlines, argument boundaries and precedence |
| Prompt AgentsMd, AddDir, Unsupported | Reuse resolver/materialization; verify whether resolved source is file or directory | Directory variants plausible; any resolved file inherits blocker | Every matrix variant, including deliberately unsupported prompts, not silently invented support |
| Image HOME, USER, WORKDIR, ENV, ENTRYPOINT/CMD | Stored metadata and Mac guest-default tests passed | Mac local-archive runtime defaults failed; command-less foreground launch rejected | Fix smolvm metadata resolution; verify awman's explicit overrides, supplementary groups, ownership |
| Streaming execution, stdin, exit status | CLI/SDK surfaces available | CLI/SDK surfaces available | ACP binary-clean stdout, stderr separation, EOF, cancellation, no deadlocks under output load |
| TUI terminal/reattach | PTY/exec support in source | PTY/exec support in source | Resize, Ctrl-C, detach without killing agent, reconnect after awman restart |
| Background execution, stop/remove, discovery/stats | Needs complete adapter, not Docker argv substitution | Same; persistent machine API useful | Labels, session ownership, crash recovery, stop grace, idempotent cleanup, stats units |
| Squads/multiple sessions | Potential fit once arbitrary mounts/lifecycle validated | Do not advertise mount capability while file cases are missing | Concurrent agents, naming, per-workspace isolation, stale-session repair, shared auth refresh |
| CPU/memory | vCPU integer != Docker fractional CPU quota | Same; advertised elastic memory is not automatically an awman hard-limit guarantee | Define conversions explicitly, test OOM/process cleanup; reject unsupported semantics rather than silently round |
| Network, DNS, localhost services, proxies | Policy defaults differ from Docker | Egress is opt-in; host loopback differs from guest loopback | Provider auth/API, enterprise CA/proxy, local MCP services, DNS and denied egress |
| `--allow-docker` / Unix sockets | Requires deliberate host-socket bridge, not ordinary file sharing | `--docker-socket` exposes guest dockerd to host: **opposite direction**, not parity | Explicit opt-in host privilege escape; socket bridge and denied-by-default behavior |
| Rootfs persistence, filesystem toolchain | Linux guest avoids host ABI mismatch | Same, with extra guest image processing | Git lockfiles, hardlinks/symlinks, xattrs, executable modes, watchers, case sensitivity, large repos |
| Agent-internal sandboxing | Guest policy/kernel must support it | Guest policy/kernel must support it | Run actual agent sandbox/bwrap-style namespace/seccomp cases; don't disable safeguards to make tests pass |
| Image readiness/build UX; `src/engine/container/runtime.rs` | Needs import/prepare path rather than invoking build | Same | Missing image actionable error, no automatic Docker/Apple dependency for ordinary startup |

Microsandbox's [single-file implementation](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/crates/filesystem/lib/backends/singlefilefs/mod.rs) exposes a synthetic directory for the selected file, with cache invalidation settings for host updates. Its [runtime mount setup](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/crates/runtime/lib/runner/vm.rs) passes read-only flags to host filesystems; [write operations](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/crates/filesystem/lib/backends/passthroughfs/unix/file_ops.rs) check them. This is stronger source evidence than a guest mount flag alone, but still requires adversarial VM checks. Smolvm's directory-only validation is in [HostMount](https://github.com/smol-machines/smolvm/blob/c890ec3dae904ca9d47e64b084a23cfbf2230849/src/data/storage.rs#L95).

The supplied mount probe tests synthetic forms of all four overlay categories, all settings-mount strategy families, prompt-file/AGENTS.md/add-directory delivery, nested read-only mounts, rename/hardlink/execute behavior, symlink escape and atomic credential refresh. It uses root deliberately for the read-only/remount challenge. It is **not** an end-to-end invocation of awman's overlay resolver or every agent binary, and does not replace non-root tests. Do not “fix” smolvm by exposing each file's whole parent directory or replacing live files with one-time copies: that changes both isolation and refresh semantics.

## Implementation options after hardware validation

1. **Bundled Microsandbox worker — recommended first path.** Awman owns a pinned runtime/firmware bundle, private state and transport; users install neither msb nor Docker/Apple for execution. Preserve current container option/config resolution and implement a full new backend adapter. Resolve the Linux `libcap-ng` packaging issue, macOS signing, secret transport and importer strategy before calling it dependency-free. Pros: shortest path to required mount/image behavior; subprocess contains global runtime state and avoids SQLite collision. Cons: asset extraction/update/signing and process supervision; not a literal one-executable process tree.
2. **In-process/reexec Microsandbox integration.** Align SQLite dependencies or extract narrower crates, and make awman the worker entrypoint. Pros: tighter APIs, possible one executable plus firmware assets. Cons: more upstream coupling, larger native build/release burden, and no demonstrated advantage for the user's actual compatibility needs yet.
3. **Patched smolvm integration.** Add safe live single-file sharing, support/normalize OCI archives, preserve local-archive runtime defaults, resolve default commands, fix the flattener, then align the SQLite graph or bundle a worker. Pros: good reexec architecture and machine lifecycle features. Cons: more prerequisite work, especially security-sensitive filesystem work; current release fails hard requirements before awman integration starts. Revisit if these capabilities land upstream.

For either choice, refactor the CLI assumptions in `src/engine/container/backend.rs` and `runtime.rs`: `cli_binary()`, Docker-shaped default background/exec/stop/remove commands, image readiness and image HOME inspection cannot simply be pointed at another executable. Keep agent/config policy in awman and keep the worker's protocol versioned. Build support remains deliberately out of scope.

## Go/no-go gates before a work item is finalized

- Corrected Mac baseline obtained. Repeat native Linux ARM64 and x86_64 with accessible KVM. Validate exact signed release artifacts, not only development builds.
- Complete actual local Apple and local/remote Docker-store import round trips, plus authenticated/local registries and offline relaunch. Settle whether an optional source CLI is acceptable or a bundled source bridge is mandatory.
- Pass file/dir ro/rw, nested mounts, non-root ownership, atomic token refresh and host-sibling/parent escape checks. A single missing overlay/config strategy is not full compatibility.
- Add and run PTY/resize/reattach, stdin/ACP framing, lifecycle/recovery, squad concurrency, resource enforcement, networking/proxy/CA and realistic repository workloads.
- Audit malformed OCI archives, symlink/hardlink extraction, compressed-size limits, image/config secret persistence, helper update integrity and firmware/guest asset distribution obligations. The tiny trusted-fixture builder is not a hardened importer.
- Measure cold/warm startup, peak/idle RSS, disk growth, clone/git/npm/cargo workloads and multi-agent scaling on real hosts. No performance ranking is justified by archive size or marketing cold-start claims.

No work item was created. Current outcome: **Microsandbox is the leading candidate; smolvm requires concrete fixes; final compatibility sign-off is waiting on hardware tests and image-store integration decisions.**
