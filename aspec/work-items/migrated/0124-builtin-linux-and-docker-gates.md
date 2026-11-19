# Work Item: Task

Title: Enable and validate the builtin runtime on Linux x86_64 KVM
Issue: https://github.com/prettysmartdev/awman/issues/47
Status: In progress; [PR #75](https://github.com/prettysmartdev/awman/pull/75) targets `nextgen`.

## Summary

Enable the experimental builtin runtime in Linux x86_64 GNU builds and release
assets, with real KVM acceptance and a repeatable `make linux-prerelease` target.
Repair the Linux developer installation path and source/test portability defects.
The user confirmed x86_64-only scope on 2026-11-19; Linux ARM64 remains gated.

This replaces the old optional WI 0124 plan. WI 0144's manifest suites and
`awman-test-support` are the only supported test path. Historical native scripts,
old harness names and passing skips are not current acceptance evidence.

## User stories

As a Linux developer, I want the installed binary to include its microVM runtime
and import supported image sources, so running agents needs neither a separate
runtime installation nor a Docker daemon after image acquisition.

As an awman contributor, I want a clean checkout to build and install normally,
and one prerelease command that captures verifiable native evidence for future
releases.

## Implementation details

- Include builtin support in GNU/Linux x86_64 source builds and the normal
  `awman-linux-amd64` release. Keep ARM64, Windows and musl builds on their
  existing backends. Linux runtime startup requires usable `/dev/kvm` and
  never falls back to host execution.
- Verify native x86_64 kernel/Agentd payload hashes. Use libkrun's existing
  private-memory boot mode for fresh x86_64 VMs, so the process-owned embedded
  kernel allocation is never shared writable guest RAM. Preserve independent
  checkpoint-restore ownership. Create inherited Unix lifecycle locks as `0600`
  regardless of umask, while retaining strict worker descriptor validation.
- Build release assets on Ubuntu 22.04 and reject glibc requirements newer than
  2.35. Embed the VM, kernel, guest agent, SQLite and other runtime dependencies;
  only the documented operating-system C ABI and unwinder remain dynamic.
  Keep musl static build regression coverage. Use Thin LTO and stripped symbols.
- `make build` fetches missing checksum-pinned payload inputs. `make install`
  builds as the invoking user and atomically replaces the destination, including
  a currently executing inode; escalate only the final installation if needed.
- Create new data/config directories with private permissions even under a
  group-writable Linux umask. Keep existing unsafe-directory refusal intact.
  Test fixture roots must also be private at creation; guest cleanup audits the
  owned catalog without trying to host a builtin worker in a test executable.
- Support explicit archive, OCI registry and Docker Engine sources. Validate
  Docker Unix/TLS/mTLS transport, Docker-save and OCI archives, authenticated
  private-CA registries, HTTP/CONNECT proxies, proxy authentication/NO_PROXY,
  bearer renewal, retries, disconnects, digest failures, deadlines and cancellation.
  Late failures must publish neither a successful import nor a usable cache entry.
  Apple Containers remains a macOS-only source with explicit refusal on Linux.
- Validate Docker builder preparation, import, rebuild and cached guest execution.
  Keep source identity fixed and never select an unrelated source or runtime.
  Imported images run without the source daemon or builder. Shipped Dockerfile
  templates and Docker runtime regression contracts remain required boundaries.
- `make linux-prerelease` uses the existing native-validation runner. Preflight
  architecture, KVM, Docker Unix socket, umask 0002 and enforced offline seccomp;
  prepare private, independently hashed OCI/Docker fixtures and an owned loopback
  registry; enumerate registered tests independently of their results; validate
  the exact candidate and archive a packet with source/artifact/input digests.
  Cleanup owned services on failure. Missing infrastructure fails the run.
- The first Linux builtin release has no published predecessor. Prove cached
  state across executable replacement without claiming a cross-release upgrade.
  Once `previous_release.x86_64-unknown-linux-gnu` is pinned, the runner must
  automatically require the genuine old/new guest and offline artifact tests.
- Keep issue #46, all carried patch provenance/removal conditions, and saved
  upstream proposals current. No upstream issue or PR is submitted in this work.

### Coordinated assignments

The build/runtime coordinator owns release assets, installation and kernel memory
integration; the native-validation coordinator owns manifest/packet/CI contracts;
the Docker/source coordinator owns real-service and failure-boundary tests.
Record native architecture, KVM access, source revision and artifact identity
before accepting evidence. Only the supplied x86_64 machine earns Linux native
acceptance; ARM64 host-only compilation earns none.

## Edge cases

- Unavailable/denied KVM, wrong architecture, missing or corrupted payloads.
- Concurrent guests, repeated boot after failure, private mounts, permissions,
  resource limits, network isolation, owner exit and process cleanup.
- Atomic installation while an executable is in use; cached restart; an old
  process resolving a deleted executable must require a restart explicitly.
- Minimal PATH and enforced-offline execution; no extracted host helpers or
  ambient Microsandbox configuration/runtime discovery.
- Source credential failure or cancellation after partial bytes; no secret
  diagnostics, stale success reports, fallback source or partial cache reuse.
- A format accepted by the builtin importer is not necessarily accepted by
  Docker's image loader; fixture formats and their hashes must remain explicit.

## Test considerations and evidence

- [x] Host preflight: native x86_64, ordinary-user KVM access and Docker;
  native payload extraction and kernel accessor diagnostic.
- [x] Exact pinned libkrun memory tests: independent sibling mappings, immutable
  backing, sparse/private fresh memory; five tests, no skipped cases.
- [x] Registered registry boundary suite: ten real-registry tests covering the
  transport/auth/proxy/retry/failure matrix above. Full native rerun remains required.
- [x] Manifest/tooling checks and release ABI parser/policy tests.
- [ ] Full native guest suites: boot, exec, import, network, upgrade/replacement,
  builder and strategy.
- [ ] Native Docker source, runtime conformance and shipped-template suites.
- [ ] Exact optimized/LTO/stripped artifact: offline boot, dependency/ABI scan,
  protocol policy, entrypoints, smoke, measurements and executable replacement.
- [ ] Real `make install` on the Linux host and installed runtime diagnostic.
- [ ] Independently verify and retain native packet; update evidence register.
- [ ] Final `nextgen` PR CI green and ready for review/merge.

Source revisions and full packets must accompany final acceptance; intermediate
passes do not certify a later implementation. No ARM64, Windows or Docker
Sandboxes native pass is claimed by this x86_64 work.

## Codebase integration

Follow `aspec/architecture/design.md`, `aspec/architecture/security.md`,
`aspec/devops/testing.md` and `ci/gates.toml`. No pre-WI144 harness or fallback
workflow is restored. Keep unsafe code in the host-shims boundary. Maintain
published-crate archive hashes, minimal diffs and independent removal gates.

## Documentation

Update current runtime, configuration, getting-started and isolation guides with
Linux availability, glibc/KVM requirements, image sources and Docker builder use.
Document contributor prerequisites and prerelease packet review in `aspec/devops/`.
Implementation and upstream patch notes stay in this work item and `third_party/`,
not in a work-item-specific user guide. Historical release notes remain historical.
