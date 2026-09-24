# Microsandbox strict-embedding feasibility spike

Date: 2026-09-24. Scope: Microsandbox only; smolvm is no longer a candidate. No awman application source, dependency manifest or work item is changed.

## Straight answer

**Strict single-executable embedding is demonstrated by the Apple Silicon prototype.** A standalone executable containing the full Microsandbox SDK/CLI/VM stack, its guest agent and its Linux kernel built successfully, imported OCI, re-executed itself as the VM worker, booted the guest and passed all nine image assertions and all 32 mount/config assertions. The runtime used its embedded kernel, linked only macOS system libraries/frameworks, and produced no extracted-helper candidates in the tested state directories. A Linux ARM64 build also imported OCI and reached VM initialization without extracted helpers.

**Linux guest boot remains untested.** This development host lacks `/dev/kvm`; its probe reaches the embedded-kernel provider and then fails while opening KVM. Unlike the earlier upstream-msb Mac checks, the latest Mac result exercises the custom strictly embedded executable. This establishes the packaging/boot approach on Apple Silicon, not complete production readiness or support for every awman workflow.

The original strict spike does not link into the actual awman application: it deliberately excludes awman's database graph. **Follow-up:** the [SQLite resolution spike](../sqlite-resolution/README.md) now links actual awman library code and the full strict runtime in one Linux ARM64 executable, using an already-merged one-line SQLx backport. It passes awman persistence regressions and msb database checks without downgrading rusqlite. Production entrypoint/backend integration and the combined Mac build remain outstanding.

## Apple Silicon strict-embedding result

User-supplied archive SHA256: `aab74bec2a36e46c35eeaef45a2e2ffb5222a61e2bd5e284727aad3e42c54060`. Selected raw outputs are retained under `results/mac-arm64/`. Rust and Cargo versions were 1.94.0; the unoptimized full build completed in 1 minute 15 seconds on the user's Mac. That build time is an observation, not an application-startup measurement.

- OCI import and inspection succeeded with an empty host executable search path.
- Image ENTRYPOINT/CMD ran; USER, GID, WORKDIR, HOME, ENV, ordinary/opaque whiteouts, upper-layer contents and executable mode all passed.
- Exit status 37 propagated correctly through the self-executing runtime.
- All 32 mount/config assertions passed, including individual file ro/rw mounts, staged Claude config, prompt delivery, nested read-only sharing, synthetic escape/remount cases, and live atomic credential refresh. Guest stdout completion and stderr markers were present.
- Runtime logs for both the image and exit tests recorded `embedded-kernel-provider-called bytes=24576000`. The scan for host msb/agentd executables and firmware shared libraries in the tested state directories was empty.
- `otool -L` listed only macOS system frameworks/libraries: CoreFoundation, CoreServices, Hypervisor, Security, SystemConfiguration, libiconv and libSystem. No bundled firmware or VMM DSO was linked.
- The probe was ad-hoc signed using a plist containing only `com.apple.security.hypervisor`. The successful run did not require an explicit disable-library-validation entitlement. It does **not** validate a notarized hardened-runtime release; the script did not enable hardened runtime.

These results support proceeding with a proper embedded-kernel API patch and awman integration. They do not eliminate the remaining Linux/KVM, dependency-alignment, image-store, lifecycle/PTY, release-signing or security-review work. No further repeat of this identical Mac suite is needed.

## What “strict” means here

- Exactly one **host application executable** per target: `awman` contains its normal application, runtime code, guest-agent bytes and kernel bytes.
- A VM runs in a **separate process re-executing that same awman executable**, not inside the foreground TUI/API process. One file does not mean one process.
- No extracted host worker executable, firmware DSO, helper script, or separately installed container runtime is required for the tested core path.
- OCI downloads, layer caches, writable disks, state databases, sockets and logs still exist on disk. A Linux guest still executes its own programs, including the embedded guest agent; that is not executing a separate host helper.
- macOS frameworks, Linux KVM, and normal OS ABI libraries remain prerequisites. This is not a claim of a statically linked libc, universal Linux-distribution compatibility, or no kernel prerequisites.
- This packaging question does not solve CLI-free Docker/Apple source-store adapters. Those remain shared backend work, separate from embedding the runtime.

## Experiments and results

Pinned sources: Microsandbox `v0.7.2` / `60d4dc8a436fb9365491567ec21d073e924e3c6d`; `msb_krun 0.1.39`, whose published crate identifies libkrun commit `2bd0f84ad0956f3032e0490d3b8512b6851eca12`. Rust 1.94.0, Linux ARM64.

| Experiment | Observed result |
| --- | --- |
| Extract kernel data and boot addresses from the verified release firmware at build time | PASS: 24,576,000 bytes; guest load address and entry address both 2,147,483,648 |
| Compile low-level Rust VMM with the kernel in an aligned static byte array | PASS |
| Link native Linux capability handling without a runtime `libcap-ng.so` dependency | PASS using a build-time static archive |
| Build full SDK + CLI command handlers + runner + embedded guest agent + embedded kernel | PASS; no changes to Microsandbox workspace source required |
| Move only the resulting executable into a fresh runtime directory | PASS |
| Run with an empty host executable search path and isolated state | PASS for image import/inspection and worker launch |
| Import the existing OCI fixture and preserve its configuration | PASS; raw inspection JSON saved |
| SDK launches this same executable as `machine` worker | PASS: worker initializes and calls the embedded kernel provider |
| Runtime dynamic dependency inspection | Only libc, libm, libgcc_s and the Linux loader; no libkrun, libkrunfw, cap-ng or SQLite DSO |
| Runtime state scan for msb/firmware/agentd helper files | None found in the exercised path |
| Linux guest boot | BLOCKED: KVM open fails with ENOENT; not a guest compatibility result |
| Mac strict build, symbol export, signing and guest boot | PASS in subsequent user-run script; 9 image and 32 mount/config assertions passed |
| Linux x86_64 strict build/boot | NOT RUN |

The source modification is one small patch to `msb_krun`'s firmware loader; `embedded-loader.patch` records it. For this probe, when the configured firmware path is the current executable (or the low-level probe's sentinel), the loader resolves `krunfw_get_kernel` from the process itself rather than opening a firmware DSO. The supplied function returns pointers into a 64-KiB-aligned, process-lifetime kernel byte array. The usual kernel-bundle validation and VM construction remain intact.

The build-time extractor calls the release firmware's existing getter, preserving its load/entry addresses and exact kernel bytes. That shared library is a **build input**, not a runtime dependency. A production pipeline can instead generate the same data directly from the pinned kernel build; rebuilding a kernel is not necessary to run the probe.

The full probe reuses the upstream image, run, remove and `machine` command implementations. It points `MSB_PATH` at itself and, as a temporary compatibility shim, points `MSB_LIBKRUNFW_PATH` at itself too. The SDK's current pair resolver accepts two existing file paths; its embedded `.msbver` / Mach-O version section identifies the runtime independently of the application's version. Thus no SDK fork was needed to test the control flow. **An executable masquerading as a firmware path is a probe technique, not the recommended public API.**

Evidence is in `results/linux-arm64/` and `results/mac-arm64/`. The full unoptimized, debug-info-disabled executable measured 243,895,432 bytes on the Linux host. That is not a release-size prediction or a fair comparison with the optimized upstream archive. Release/LTO size, RSS and startup impact have not been measured. Do not infer performance wins from eliminating extraction.

## Why the pieces fit

### Host VM code is already Rust-linked

Microsandbox does not require a dynamically loaded host `libkrun.so` in this path. Its runner links the `msb_krun` Rust crate and builds custom filesystem/network backends in-process. The runner exposes `vm::enter(Config) -> !`. This is the real runner, including relay, heartbeat, filesystem and lifecycle machinery, not a replacement VM implementation. See [runtime manifest](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/crates/runtime/Cargo.toml) and [runner entrypoint](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/crates/runtime/lib/runner/vm.rs#L570).

### Firmware is data behind a small loader boundary

The relevant low-level loader opens a DSO and calls `krunfw_get_kernel` to obtain bytes, size and guest addresses. The VM then builds a `KernelBundle` from them. The spike replaces the source of those pointers, not kernel loading or virtualization. A production-quality solution should expose a typed, validated, process-lifetime embedded-kernel provider instead of path sentinels and process-symbol lookup. See [pinned loader](https://github.com/superradcompany/libkrun/blob/2bd0f84ad0956f3032e0490d3b8512b6851eca12/src/krun/src/api/vm.rs#L710).

### The guest agent already supports byte embedding

`microsandbox-filesystem`'s `embed-binaries` feature uses `include_bytes!` for the Linux agent and presents it through guest filesystem machinery. It need not be materialized as a host program or invoked on the host. Crucially, this feature differs from the **SDK's** similarly named `embed-binaries`, which embeds an archive of host executables/libraries for extraction. The strict probe enables guest-agent embedding but not that SDK host-bundle feature. See [guest payload selection](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/crates/filesystem/lib/agentd.rs).

### Separate process, same executable

The runner takes over its process and exits it when the VM ends. Starting it as a thread in the awman TUI/API process is not an acceptable shortcut: guest shutdown or runner failure could terminate awman, and VM globals and signal handling would share that process. Re-exec preserves per-VM process isolation and allows multiple independent VMs. Do not replace re-exec with a fork-only child of awman's multithreaded runtime.

The existing launcher transfers private configuration and inherited descriptors to `machine`, retains lifecycle ownership, and receives startup information. Reuse that contract rather than rebuilding supervision. The probe's version section also avoids hijacking awman's ordinary `--version`. See [SDK spawning](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/sdk/rust/lib/runtime/spawn.rs#L683) and [machine command](https://github.com/superradcompany/microsandbox/blob/60d4dc8a436fb9365491567ec21d073e924e3c6d/crates/cli/lib/machine_cmd.rs).

## Production work still required

1. **Dependency alignment.** Apply the [selected SQLx manifest backport](../sqlite-resolution/README.md), preserving awman's rusqlite/native SQLite baseline. The follow-up combined probe validates this resolution on Linux ARM64; production entrypoint and other platform builds still need their integration gates.
2. **Explicit embedded-runtime configuration.** Model self-executable launch and embedded firmware as real SDK/runtime options. Preserve existing external-runtime behavior. Disable implicit host-bundle installation, version replacement and ambient override discovery for awman's builtin backend. Keep protocol/version capability discovery separate from awman's user-visible version.
3. **Typed kernel provider.** Validate architecture, alignment, size, addresses and lifetime; ensure LTO/dead-strip cannot remove data or the provider. Keep normal firmware loading for upstream compatibility. The proof's exported symbol and fake firmware path should not become a permanent public interface.
4. **Internal worker dispatch.** Dispatch before normal awman/Tokio/TUI initialization; preserve inherited FD ownership, private config transport, startup errors, watchdogs, logging and cleanup. Do not expose the entire experimental CLI as awman's public command surface. Prefer a narrow adapter over permanently depending on every CLI command.
5. **Native build/release assets.** Pin kernel and guest-agent payloads per architecture, package notices/source obligations, and make builds reproducible/offline after fetching verified inputs. Linux must statically link or remove the cap-ng dependency for strict packaging; the probe proves static linking works, but its license/distribution requirements need review. This does not make the implementation exclusively Rust.
6. **Mac distribution.** The ad-hoc-signed probe passed with only `com.apple.security.hypervisor` in its entitlement plist. Test the final optimized awman artifact under the intended hardened-runtime, signing and notarization settings; those release checks are not established by the successful ad-hoc build.
7. **Upgrade/lifecycle testing.** Running old awman workers may outlive an application update. Verify reattachment, catalog migration, process ownership and stale-state cleanup across versions. Embedding ties runtime/kernel updates to awman releases, unlike an independently replaceable bundle.
8. **Full backend contract.** All common awman adapter work remains: overlays/config staging, real store importers, PTY/ACP streams, resource/network behavior, squads and frontend lifecycle. Strict packaging does not remove those tasks or prove them through the tiny probe.

## Benefit versus effort

Estimates below are engineering judgment, not measured delivery times. They assume one engineer familiar with Rust/macOS builds, resolved awman dependency conflicts and reusable importer/backend work. The Mac proof has now succeeded. They are **incremental packaging/runtime integration effort**, not the full feature schedule. If Linux validation uncovers platform issues or upstream APIs must be substantially forked, re-estimate.

| Approach | What ships/runs | Incremental effort estimate | Benefits | Costs |
| --- | --- | --- | --- | --- |
| Embedded bundle, extracted msb + firmware | One download; multiple privately installed host artifacts | Roughly 3–7 engineer-days | Closest to tested upstream release, minimal fork; easiest fallback and upstream updates | Extraction/install races, writable executable cache, extra signing/artifact checks; not strict single-executable |
| Self-reexec with external/extracted firmware | One host executable plus firmware DSO | Roughly 1–2 engineer-weeks | Removes separate worker executable; validates worker integration incrementally | Still violates strict requirement; retains firmware discovery/extraction and dynamic-library packaging |
| **Strict self-reexec + embedded kernel/agent** | **One host executable; one same-executable child per VM** | **Roughly 2–4 engineer-weeks** | No executable/DSO extraction or matching-pair installation, atomic application/runtime distribution, direct Rust integration | Small maintained/upstreamed loader/API changes, larger unified build/signing surface, tied runtime updates, dependency alignment and cross-version testing |
| VM threads inside awman's main process | One executable and no separate VM worker process | Not recommended; much larger redesign | Saves a process boundary only | Conflicts with runner exit/global-state assumptions; crashes/shutdown threaten the application; poor multi-VM separation |

The strict design is **not a VMM rewrite**. The proof needed a small loader patch plus a thin dispatcher and payload-generation step. Most production effort is making those boundaries explicit and shipping/testing them reliably, not implementing virtualization.

The strongest benefit over a bundle is artifact ownership and deployment simplicity, not stronger guest isolation or guaranteed faster execution. Both designs can use the same kernel, guest agent, filesystem backends and per-VM process boundary. The bundle remains the quickest, lowest-divergence option. **Given awman's stated preference and the successful Mac proof, I recommend strict self-reexec, retaining the bundled option as a fallback.**

## Reproduce the Mac proof

Use the previous successful fixture directory; no smolvm download or test is performed:

```bash
bash tools/oci-runtime-spike/strict-embed/mac-checks.sh /private/tmp/awsp.8uJ29q
```

This differs from the previous smoke test: it **builds a custom runtime executable from Rust source**, so an existing recent toolchain (tested locally with Rust 1.94), Xcode Command Line Tools, internet access and several GB of build space are needed. It may take several minutes. It checks the previous release archive hash, fetches pinned source/agent/crate inputs, extracts firmware bytes only at build time, applies the included patch in scratch, and ad-hoc signs only the new probe executable. No sudo, global installation, real credentials or awman source edits occur.

The runtime phase copies only the executable into its own directory, uses isolated state and an empty executable search path for image/exit tests, and reuses the prior synthetic OCI fixture. It runs the image contract, exit-code check and Microsandbox mount suite. The mount harness retains its normal system PATH for its shell orchestration; that subtest is not an empty-PATH proof. The report collects dynamic-library dependencies, embedded-provider markers and candidate extracted-helper paths. Retaining build inputs elsewhere is not itself evidence of runtime access; provider markers and absence of runtime library dependencies establish which firmware path was used. A later syscall/file-access audit should test the final production artifact.

Review the printed `results.tar.gz`. If compilation fails, `logs/build.log` contains the errors; the script prints the final lines. The archive excludes binaries, images and VM disks, but logs can contain local paths. **This script's Mac build/sign/boot and compatibility checks have now passed in the user-run experiment recorded above.**

## Linux reproduction notes

The full and low-level probes have separate locked manifests. `full-probe` links the actual SDK/CLI/runtime; `probe` isolates the VMM/kernel boundary. Build prerequisites are Rust, a C compiler/linker, and a static cap-ng archive on Linux. `extract-kernel.c` reads the verified platform-native firmware using its exported getter; do not execute an untrusted firmware library.

1. Obtain the pinned Microsandbox checkout and published `msb_krun-0.1.39.crate` (SHA256 `0944407a6ae125935e64dcf9be666e56d2cca0907b9e34be6648784db453490c`). Apply `embedded-loader.patch` to the unpacked crate.
2. Compile `extract-kernel.c` with `cc ... -ldl`; invoke it with the firmware library and output kernel path, redirecting stdout to a metadata file.
3. Put the verified architecture-matching `agentd` in a build-artifacts directory. Set `SPIKE_KERNEL`, `SPIKE_KERNEL_METADATA` and `MSB_EMBED_ARTIFACTS_DIR` to those paths.
4. Build `full-probe/Cargo.toml` with Cargo patches mapping `microsandbox-cli` to the pinned checkout's `crates/cli` and `msb_krun` to the patched crate. Pass `cargo rustc ... -- -L native=/directory/containing/static/libcap-ng.a`; avoid a dynamic linker stub in that directory.
5. Inspect `ldd`/`readelf`, copy only the resulting executable into a short private runtime directory, and use `env -i PATH=/empty MSB_HOME=/short/private/state MSB_BACKEND=local /path/to/probe image load ...` followed by `run`. Never override your real HOME/CODEX_HOME or mount real credentials.

The Linux ARM64 experiment used Debian's `libcap-ng-dev_0.8.3-1+b3_arm64.deb`, extracted into scratch rather than installed (SHA256 `92ac2d723583ac9a34340f00c61adbf6a3ae613ec395541bc32d428f6c16c092`). Only its static library was used as a build input. The chosen proof is therefore reproducible without changing the machine's system packages; a production build should own a documented source-based native-dependency pipeline.
