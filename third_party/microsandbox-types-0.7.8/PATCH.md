Published crate: `microsandbox-types 0.7.8`, archive SHA-256
`74af7e859dcfb714bf612e7e27ad682aaedc946be419ace40e661d7e6ae74ba6`.
Upstream revision: `7b7b9dc89e9a1c77801918f7833c3381d1754579`.
The manifest declares Apache-2.0; `LICENSE` is the Apache-2.0 text of the
upstream repository at `v0.7.8` (the published crate ships no LICENSE file;
byte-identical to the file carried by the 0.7.2 trees).

Apply `patch -p1 < upstream.diff` to the extracted published archive.
Patch SHA-256: `f0db5e8555259e07288a97a45a4d3dcb66a561a98cd4c1229bbd26b0f4b0fa41`.
Three files add default-false `strict_sni` to NetworkSpec and CloudNetworkSpec
and preserve it in their conversions. This is the typed transport companion
to the network crate patch; without it SandboxBuilder serializes the network
configuration through NetworkSpec and silently loses the opt-in. It does not
claim an unpatched cloud server understands or enforces this local extension.

Rebase notes (0.7.2 -> 0.7.8): `NetworkSpec::default()` and
`CloudNetworkSpec::default()` now set `strict: true`; `strict_sni: false,` is
added after it in both, so only `strict_sni` defaults off. Field hunks and both
conversions in `cloud/mod.rs` are otherwise unchanged.

The actual SandboxBuilder test `sdk_accepts_every_compiled_network_policy`
asserts both the stored spec and deserialized runtime NetworkConfig retain it.
Remove together with the network patch once a pinned upstream version carries
an equivalent explicit configuration end to end and boundary/native tests pass.
