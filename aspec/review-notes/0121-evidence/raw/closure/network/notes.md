# Network closure implementation — 2026-09-28

Implemented explicit default-false strict_sni option across paired pinned
microsandbox-network/types 0.7.2 patches. Driver opts in while retaining
strict=true and TLS interception disabled. Flag survives actual SDK builder
NetworkConfig→NetworkSpec→NetworkConfig roundtrip (previously a new config-only
flag would silently disappear). Existing serialized configs default false.
Network policy name rules stay TCP-only. UDP/QUIC cannot acquire authorization
from cached DNS. Tightened suffix matching to require exact SNI hostname's DNS
binding; a sibling binding no longer authorizes a claimed name.

The precise boundary is visible ClientHello SNI + exact resolved name/IP
binding, not encrypted HTTP Host/:authority or ECH's hidden inner name.
An allowed service supporting domain fronting can carry another encrypted
logical authority; no claim that this can be detected without interception.

Logs in this directory:
- strict-sni.log: 3 new tests pass, including real Rustls TLS handshake plus
  bidirectional application data through the actual SDK TcpProxy, and missing,
  wrong/shared-IP, unresolved and sibling-bound name denials before upstream dial.
- sdk-network-all.log: initial 561 pass/1 fail due upstream test explicitly
  expecting sibling-DNS allowance. Updated this intentional security contract's
  expected verdict to deny; independent no-dial proxy case confirms it.
- driver-network-roundtrip.log: actual root SDK builder roundtrip test passes.
- sdk-network-all-final.log and enforced-network-script.log: final suite results.

Reproducer: PATH=/usr/local/cargo/bin:$PATH CARGO_BUILD_JOBS=2 bash
 tools/test-builtin-network.sh. The runner uses a temporary source copy with root
Cargo.lock, offline resolution, local type/krun patches and shared build target,
and calls isolated-test.sh with service opt-ins forced off. It explicitly
selects strict_sni, strict_mode_blocks_hostname_allowed_opaque_tls and
model::policy::types::tests (reviewed
pure policy or loopback-only tests). The full upstream suite is separate.
It does not modify root or vendor lockfiles. Root owns Makefile wiring.

Patch provenance/license/reconstruction/removal gates live in both vendor
PATCH.md files. New patch hashes network 993d00b0d99e436e2cebc72a8a5ccbadcfb458b8ba1d9ad4cb242bafdcc2e100;
types 30bf86e5a7e58acbe889d3d05eb898795b3078dedc608d0435b252196c6eed4f.

Follow-up resource requirements: Apple Silicon HVF and Linux aarch64/x86_64 KVM
native runners with current payloads and controlled DNS/TLS endpoints. Run
actual non-root guest positive HTTPS allowed authority; wrong/missing SNI,
shared-IP, direct-IP, sibling-name DNS binding, DNS rebinding and UDP/QUIC
negatives; authorized versus unauthorized host ports; custom guest CA/auth,
MCP/proxy, denied-egress and concurrent VM isolation. Record guest→stack
execution; host TcpProxy tests alone do not prove packet routing or isolation.

Final observed results: full vendor suite 562 passed, 0 failed/ignored; actual
SDK builder roundtrip 1 passed. Both upstream.diff files dry-run cleanly against
the published archives (patch-reconstruction.log). Final runner also passes
bash -n. The isolated subset command was queued while shared-target compiler
work was active; root's final make test-builtin must execute the final filter
set and record its result. The final filter intentionally excludes upstream
strict_mode_leaves_default_allowed_opaque_tls_to_policy, which assumes no
ambient service listens on 127.0.0.1:9.

Documentation follow-up complete: docs/11-runtimes.md and security.md now state
the implemented visible-SNI/exact-DNS-IP boundary, end-to-end TLS limitations
(encrypted Host/authority, domain fronting, ECH inner names), TCP-only name
rules and still-unverified native execution. Removed stale allowed-HTTPS
refusal-defect claims from these owned documents. third_party/README.md,
NOTICE.third-party.md and WI0120 now enumerate six patches, including exact
network/types archive/diff hashes, shared upstream revision, licenses,
reconstruction and paired removal gates.

The earlier isolated runner completed: 88 passed, 0 failed/ignored (its filter
also included one upstream port9-assumption test). Final enforced runner was
narrowed to exclude that ambient-dependent test; root's final gate must record
the resulting 87-test execution. No implementation changes during doc update.

Root final gate: `make test-builtin` executed the final narrowed isolated SDK
runner successfully: **86 passed, 0 failed, 0 ignored, 476 filtered**. The earlier
88-count command included additional upstream strict-mode tests; neither count
is native guest evidence. Root's final log is `../test-builtin.log`.
