# Owned dependency patches for WI 0119, WI 0120 and WI 0121

All sources come from immutable published crates. Their `PATCH.md` files record archive hashes, original licenses, exact `upstream.diff`, rationale and removal gates. Reproduce a patch by downloading the named `.crate` from `https://static.crates.io/crates/<name>/<name>-<version>.crate`, verifying SHA256, extracting it, and applying `patch -p1 < upstream.diff` in the extracted crate root. Root `Cargo.toml` selects these sources through repository-relative `[patch.crates-io]` entries; `Cargo.lock` fixes the graph. No Cargo cache edits or mutable branches are part of the build.

| Patch | Purpose | Removal path |
|---|---|---|
| `sqlx-sqlite-0.9.0` | One-line native SQLite bound backport, upstream `94aafe3a68884d923b0798a767c8d7f6cfda89d2`; silences `time` 0.3.55 deprecation warnings | Compatible SQLx release plus combined-link, store, migration and catalog checks |
| `microsandbox-filesystem-0.7.8` | Target-aware guest-agent build input (`MSB_EMBED_ARTIFACTS_ROOT/<target-arch>/agentd`) | Equivalent upstream build input API |
| `microsandbox-0.7.8` | Isolated local backend builder (`build_lazy_isolated`) that ignores installed Microsandbox config | Pinned upstream isolated builder plus hostile-config and native coexistence checks |
| `microsandbox-network-0.7.8` | Explicit visible-SNI enforcement without interception; exact DNS binding for suffix rules; guest host alias `host.builtin.internal` | Equivalent pinned upstream option plus SDK-boundary and native guest network tests |
| `microsandbox-types-0.7.8` | Preserve `strict_sni` through local/cloud NetworkSpec conversions | Remove with network patch after configuration roundtrip and native tests |

There is no `msb_krun` patch. WI 0120 removed the vendored `msb_krun-0.1.39` tree: awman depends on the published `msb_krun =0.1.41` (libkrun `b35c7e118d7a1e48c8fad103b258e64ff5c49147`) with its upstream `static-krunfw` feature and supplies the `krunfw_get_kernel` callback itself (`crates/awman-host-shims/src/kernel.rs`). Five crate patches remain: SQLx, plus the four Microsandbox trees below, all rebased from 0.7.2 onto published 0.7.8.

## Patch hashes

Microsandbox trees are published `0.7.8` crates from upstream revision `7b7b9dc89e9a1c77801918f7833c3381d1754579` (tag `v0.7.8`), Apache-2.0. Hashes below come from each tree's `PATCH.md` and match `sha256sum` of the files.

| Tree | Published crate SHA-256 | `upstream.diff` SHA-256 |
|---|---|---|
| `microsandbox-0.7.8` | `d4e2876d9ac789a99f9c7641be568ee8996d78eeebd19aa524aa2662273b3e44` | `196167178dc241f18d15aa55ed0abb76e67565a181dbf6e3a319afa46c441e42` |
| `microsandbox-filesystem-0.7.8` | `e890749fd6c87b7966865a4f7a0add888926adce222fdd4525eb64e047fee30b` | `b87c1e5f67fe0b9b3044bc43ff332601233280012bd35ea4145b6b75b9feab61` |
| `microsandbox-network-0.7.8` | `9bfc80d59e60d46b44d19c60e3e9abe5c1b3b41c92cad739f661b752376902c2` | `5dbe1bbd4ee65adb2a518ded77e6737c87225dab1d4ad76a39b8c4538c17a665` |
| `microsandbox-types-0.7.8` | `74af7e859dcfb714bf612e7e27ad682aaedc946be419ace40e661d7e6ae74ba6` | `f0db5e8555259e07288a97a45a4d3dcb66a561a98cd4c1229bbd26b0f4b0fa41` |
| `sqlx-sqlite-0.9.0` (unchanged) | n/a | `22a7e9a2e4f9dbf588a514e5305dfd6f21377107bb7600c348660606d664031f` |

The Apache-2.0 `LICENSE` in each Microsandbox tree has SHA-256 `a276ca3381fefb9cde42fccae847856085c76027557d62eee83f057eb6c53433`. See [network provenance and boundary](microsandbox-network-0.7.8/PATCH.md), [types provenance](microsandbox-types-0.7.8/PATCH.md), [SDK builder provenance](microsandbox-0.7.8/PATCH.md) and [filesystem provenance](microsandbox-filesystem-0.7.8/PATCH.md).

## Notes on the 0.7.8 rebase

- The `microsandbox` builder patch is adapted to 0.7.8's fallible `BackendConfig::new(...).prepare_for_local_backend(...)`, so `build_lazy_isolated` returns `MicrosandboxResult<LocalBackend>`. The behavior has not landed upstream as of `v0.7.8`.
- The network and types patches add `strict_sni: false` after the new `strict: true` defaults. `strict` itself is upstream's default in 0.7.8.
- The filesystem patch is still needed: published 0.7.8 handles only `MSB_EMBED_ARTIFACTS_DIR`.
- Tokio's `AsyncFd::new` deprecation warnings in the pristine 0.7.8 sources are kept. The unsafe edits an earlier carried tree made there were undocumented and were not carried forward.

## Payloads and native inputs

Kernel/guest agent release assets are pinned in `msb-payloads/manifest.toml` (Microsandbox `0.7.8`, libkrun `b35c7e118d7a1e48c8fad103b258e64ff5c49147`) and fetched natively with `tools/msb-payloads/`. The release's `vendor/libkrunfw` is `21f169f7e94798c8916315f8ac6f5999d5b88566` (Linux 6.12.111), so the kernel and guest agent changed from 0.7.2. Only Linux aarch64 is verified (kernel SHA-256 `cf6cb036c7b40923588950cc8f80750617114243730668d6859216de4d52ee39`, 24,576,000 bytes, load/entry `0x80000000`). Darwin aarch64 and Linux x86_64 are **unverified** until extracted and checked on native hosts; the old Darwin provenance file is historical. Upstream 0.1.41 replaces `capng` with the pure Rust `caps` crate for capability changes in virtio-fs. Current builds do not link or require libcap-ng; `native/libcap-ng/` is retained solely for the historical 0.7.2 catalog probe. These are independent of the SQLx patch's lifecycle.
