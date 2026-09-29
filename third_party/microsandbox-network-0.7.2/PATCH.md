Published crate: `microsandbox-network 0.7.2`, archive SHA-256
`be4f1c9d36c1957b35d674a993a59c5db1741f47c96c952b491b09d9674b92d7`.
Upstream tag revision: `60d4dc8a436fb9365491567ec21d073e924e3c6d`.
The manifest declares Apache-2.0; `LICENSE` contains the Apache-2.0 text
carried by the sibling published `microsandbox-filesystem 0.7.2` crate.

Apply `patch -p1 < upstream.diff` to the extracted published archive.
Patch SHA-256: `993d00b0d99e436e2cebc72a8a5ccbadcfb458b8ba1d9ad4cb242bafdcc2e100`.
The six changed Rust files add default-off `strict_sni`, retain it through
config/builder/poll/proxy, and require the actual SNI hostname's DNS binding
for suffix allows. Ordinary upstream strict opaque-TLS rejection remains unchanged. The exact
DNS-binding requirement intentionally tightens suffix rules in every mode;
the old upstream test that permitted a sibling binding now asserts refusal,
independently verified by the proxy no-dial regression.

With `strict(true).strict_sni(true)`, an allowed visible ClientHello SNI plus
its exact DNS name/IP binding admits end-to-end TLS without interception.
Absent or unlisted visible SNI, missing DNS binding, and a sibling hostname's
binding cannot authorize TLS via a name rule. Encrypted HTTP Host/:authority,
domain fronting behind an allowed server, and ECH's hidden inner name are
outside this observable boundary. An allowed ECH outer name is only evidence
of that outer name. Name rules must remain TCP-only; QUIC is not inspected.

Tests include a real Rustls handshake and application-data exchange through
`TcpProxy`, negative requests that never dial a local listener, serde
backward compatibility, and the existing strict-mode deny tests. Run
`bash tools/test-builtin-network.sh` from the awman repository to exercise the reviewed policy, strict-mode and SNI boundary
unit tests with the application's lockfile and native patches.
The root SDK builder test `sdk_accepts_every_compiled_network_policy` verifies
that Microsandbox's separate NetworkSpec roundtrip preserves the flag;
this needs the paired `microsandbox-types` patch.

These host-side tests do not prove a guest packet reaches the stack or prove
platform isolation. Native KVM/HVF positive HTTPS, denied egress, DNS rebinding,
shared-IP, guest proxy, UDP/QUIC, concurrent VM and CA/auth tests remain required.
Remove this patch only after a pinned upstream SDK offers equivalent explicit
SNI semantics and both SDK-boundary and native guest tests pass.
