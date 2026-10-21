Published crate: `microsandbox-network 0.7.8`, archive SHA-256
`9bfc80d59e60d46b44d19c60e3e9abe5c1b3b41c92cad739f661b752376902c2`.
Upstream tag revision: `7b7b9dc89e9a1c77801918f7833c3381d1754579`.
The manifest declares Apache-2.0; `LICENSE` contains the Apache-2.0 text of the
upstream repository at `v0.7.8` (the published crate ships no LICENSE file; the
file is byte-identical to the one carried by the 0.7.2 trees).

Apply `patch -p1 < upstream.diff` to the extracted published archive.
Patch SHA-256: `5dbe1bbd4ee65adb2a518ded77e6737c87225dab1d4ad76a39b8c4538c17a665`.
The guest host alias is `host.builtin.internal` to match awman's public runtime naming.
The policy changes in seven Rust files add default-off `strict_sni`, retain it through
config/builder/poll/proxy, and require the actual SNI hostname's DNS binding
for suffix allows. Ordinary upstream strict opaque-TLS rejection remains unchanged
(`strict` itself defaults to `true` in 0.7.8; `strict_sni` stays `false`). The exact
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

Rebase notes (0.7.2 -> 0.7.8): every hunk of the 0.7.2 patch was re-applied with
the same intent. Non-mechanical changes: `strict_sni: false,` is added after
`strict: true,` in `NetworkConfig::default()` (0.7.8 defaults `strict` to true);
the new `strict_sni` field is plain `#[serde(default)]` (not `default_true`);
the in-crate `smoltcp_poll_loop` test call in `lib/engine/netstack/poll.rs`
(new in 0.7.8) gains one `false,` argument for the new parameter.

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
