Published crate: microsandbox-filesystem 0.7.2, SHA256 `0f38a9bfe5907487f29dc265acd6ffe974ff731b0828e32a8ac0e393442ff7ed`; Microsandbox revision `60d4dc8a436fb9365491567ec21d073e924e3c6d`. License: Apache-2.0 (`LICENSE`).

WI 0119 changes only `build.rs` to check `MSB_EMBED_ARTIFACTS_ROOT/<target-arch>/agentd` before its existing input paths. This permits one repository-relative Cargo environment setting for all target architectures. The SDK's extractable host bundle feature remains disabled; this is a guest agent build input. `Cargo.toml.orig` is the unmodified published source manifest. `upstream.diff` applies to the published crate with `patch -p1`.

Remove when upstream supports an equivalent target-aware artifact root, or when awman no longer embeds through this crate.
