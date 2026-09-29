Published crate: `microsandbox-types 0.7.2`, archive SHA-256
`8f705cfd6b163fc5b00987c6cf6143b5ceed311b097e68926822b2d6daa4cfee`.
Upstream revision: `60d4dc8a436fb9365491567ec21d073e924e3c6d`.
The manifest declares Apache-2.0; `LICENSE` is the Apache-2.0 text carried
by the sibling published `microsandbox-filesystem 0.7.2` crate.

Apply `patch -p1 < upstream.diff` to the extracted published archive.
Patch SHA-256: `30bf86e5a7e58acbe889d3d05eb898795b3078dedc608d0435b252196c6eed4f`.
Three files add default-false `strict_sni` to NetworkSpec and CloudNetworkSpec
and preserve it in their conversions. This is the typed transport companion
to the network crate patch; without it SandboxBuilder serializes the network
configuration through NetworkSpec and silently loses the opt-in. It does not
claim an unpatched cloud server understands or enforces this local extension.

The actual SandboxBuilder test `sdk_accepts_every_compiled_network_policy`
asserts both the stored spec and deserialized runtime NetworkConfig retain it.
Remove together with the network patch once a pinned upstream version carries
an equivalent explicit configuration end to end and boundary/native tests pass.
