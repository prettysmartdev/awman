Published crate: `microsandbox 0.7.2`, SHA-256 `0fda3a76b3754d8eb10a5f9b6267a57517b9231eb2fe8254ab175df8a80fa8c8`; upstream Microsandbox tag revision `60d4dc8a436fb9365491567ec21d073e924e3c6d`. The crate manifest declares Apache-2.0; `LICENSE` is the Apache-2.0 text carried by the sibling published `microsandbox-filesystem 0.7.2` source.

`upstream.diff` (SHA-256 `7cd71253c1d0c7d2b13e90e957019eec1c3c4aea0929f0a5a0c254bbd86d6cb4`) adds only `LocalBackendBuilder::build_lazy_isolated`. It merges explicit builder settings into `GlobalConfig::default()` without reading an installed Microsandbox configuration. This keeps awman's builtin state and worker paths independent of an installed `msb` setup. Reproduce with `patch -p1 < upstream.diff` against the published crate.

The awman integration regression `sdk_isolated_config_ignores_installed_msb_config` runs with both hostile valid and malformed ambient config. A feature build previously failed because the method did not exist; `cargo check --locked --features builtin-runtime --bin awman` now compiles. Native coexistence and guest boot still require native KVM/HVF hosts.

Remove this patch only after a pinned upstream SDK release provides equivalent isolated-builder semantics and the hostile-config, installed-runtime coexistence, native boot, and final-artifact gates pass.
