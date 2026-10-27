**Title:** `feat(filesystem): target-aware agentd lookup for embedded builds (MSB_EMBED_ARTIFACTS_ROOT)`

**Labels:** 💗 enhancement

---

## Summary

With `embed-binaries`, `crates/filesystem/build.rs` takes the guest agent from
`MSB_EMBED_ARTIFACTS_DIR/agentd`. That's a single directory with a single file name.
It then validates the file against `CARGO_CFG_TARGET_ARCH`. Because the variable is
process-wide, every target in a build sees the same directory. So one build
environment can't supply the right `agentd` for more than one target architecture.

We'd like an optional per-architecture root:

```
MSB_EMBED_ARTIFACTS_ROOT/<CARGO_CFG_TARGET_ARCH>/agentd
```

## Motivation

We embed Microsandbox in [awman](https://github.com/prettysmartdev/awman), a CLI that ships
the VM stack, kernel and guest agent inside one binary. We keep a pinned,
hash-verified `agentd` for each architecture in the repository
(`third_party/msb-payloads/<arch>/agentd`). We point the build at them once, in
`.cargo/config.toml`:

```toml
[env]
MSB_EMBED_ARTIFACTS_ROOT = { value = "third_party/msb-payloads", relative = true }
```

- A `[env]` value is static, so it can't name a different directory per target.
  Nor can a single `cargo build` invocation with several `--target`s give each
  target's build script its own `MSB_EMBED_ARTIFACTS_DIR`.
- A wrong-architecture `agentd` is caught by validation today, which is good. But
  without this, the only option is a separate build environment, or a wrapper that
  sets the variable, for each target.

A root keyed by `CARGO_CFG_TARGET_ARCH` lets the build script pick the right file
itself, and validation still runs on the chosen file.

## Proposed change

This is additive and only used when the new variable is set. In `stage_agentd`,
before the existing `MSB_EMBED_ARTIFACTS_DIR` branch:

```rust
println!("cargo:rerun-if-env-changed=MSB_EMBED_ARTIFACTS_ROOT");

if let Some(root) = std::env::var_os("MSB_EMBED_ARTIFACTS_ROOT").map(PathBuf::from) {
    let source = root.join(&target_arch).join(AGENTD_BINARY);
    if !source.is_file() {
        panic!("MSB_EMBED_ARTIFACTS_ROOT lacks target agent: {}", source.display());
    }
    println!("cargo:rerun-if-changed={}", source.display());
    validate_agentd_file(&source, &target_arch);
    copy_agentd(&source, &destination);
    return;
}
```

When the root is unset, `MSB_EMBED_ARTIFACTS_DIR`, the workspace-local build and the
`download-binaries` path behave exactly as they do now. If the root is set but has no
file for the target, the build fails rather than falling back, so a missing
architecture is never silently replaced.

## Tests

We carry this exact diff on `microsandbox-filesystem 0.7.8`. Every embedded build we
ship (aarch64 macOS, and aarch64 Linux in development) stages `agentd` through it,
and we check the staged bytes against pinned hashes.
In a PR we'd add a build-script test, or a documented manual check, covering:
- the root with a matching architecture;
- the root missing the architecture (build fails);
- the root unset (existing behaviour).

## Questions

- Is `<root>/<target_arch>/agentd` a good layout, or would you prefer the full target
  triple (`CARGO_CFG_TARGET` or `TARGET`) as the key?
- If both variables are set, should the root win as proposed, or should that be an
  error?
