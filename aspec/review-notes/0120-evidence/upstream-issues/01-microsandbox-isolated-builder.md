**Title:** `feat(sdk): build a LocalBackend without reading user or managed config files`

**Labels:** 💗 enhancement

---

## Summary

`LocalBackendBuilder::build_lazy()` (and `build()`, which calls it) always loads the
user config file (`config_path`, `MSB_CONFIG_PATH`, or the default path) and the
machine-wide managed config through `BackendConfig::load_from`. An application that
embeds the SDK cannot get a backend made only from built-in defaults and the values
it sets on the builder.

We'd like an additive constructor, for example `build_lazy_isolated()`, that skips
both config-file layers and otherwise behaves like `build_lazy()`.

## Motivation

We embed Microsandbox in [awman](https://github.com/prettysmartdev/awman), a CLI that runs code
agents in microVMs. The VM stack, kernel and guest agent are linked into the awman
binary, and it configures the backend entirely in code. For that use case, the host's
Microsandbox config files are an unwanted input:

- **Determinism.** A user's `~/.microsandbox` config, or a managed config pushed to
  the machine, can silently change how the embedded runtime behaves (paths, defaults,
  limits). The embedding application can't see or override this.
- **Robustness.** A malformed or hostile config file makes `build_lazy()` fail, even
  though the embedder never meant to read it. Today any user who also has a broken
  standalone Microsandbox setup can't use the embedded one.
- **Coexistence.** An installed standalone Microsandbox and an embedded one should
  not read each other's configuration.

Embedders that also need to ignore runtime environment overrides (`MSB_PATH`,
`MSB_LIBKRUNFW_PATH`, `MSB_AGENTD_PATH`) can refuse those variables before calling
the constructor. That keeps the new method narrow.

## Proposed API

```rust
impl LocalBackendBuilder {
    /// Build a lazy local backend from built-in defaults and this builder only.
    ///
    /// Never reads the user configuration file, `MSB_CONFIG_PATH`, or the
    /// machine-wide managed configuration. SDK-registered runtime paths and the
    /// `MSB_PATH`/`MSB_LIBKRUNFW_PATH`/`MSB_AGENTD_PATH` overrides are still
    /// layered by `prepare_for_local_backend`.
    pub fn build_lazy_isolated(self) -> MicrosandboxResult<LocalBackend>;
}
```

The implementation follows the existing construction path. It passes defaults and an
empty managed layer to `BackendConfig::new(...)` instead of
`BackendConfig::load_from(...)`, then calls `prepare_for_local_backend(self.config)`,
so validation is unchanged:

```rust
let config = BackendConfig::new(
    GlobalConfigPatch::from_present_fields(GlobalConfig::default()),
    GlobalConfigPatch::new(),
)
.prepare_for_local_backend(self.config)?;
Ok(LocalBackend::from_backend_config(config, BackendSelectionSource::Programmatic, None))
```

An async `build_isolated()` that opens the database, mirroring `build()`, could be
added the same way if you want both.

## Tests

We carry this as a patch on `microsandbox 0.7.8` (tag revision `7b7b9dc`) with an
in-crate regression test. It writes a hostile user config and, separately, a hostile
managed config. It then asserts that `build_lazy()` rejects each one while
`build_lazy_isolated()` succeeds. The patch is about 20 lines plus the test, and
we're happy to open a PR.

## Questions

- Is a separate method the shape you'd prefer? A builder flag like
  `.ignore_config_files(true)` would also work.
- Should the isolated path also ignore the `MSB_*` path overrides, or leave that to
  the embedder as proposed?
