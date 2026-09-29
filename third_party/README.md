# Owned dependency patches for WI 0119 and WI 0121

All sources come from immutable published crates. Their `PATCH.md` files record archive hashes, original licenses, exact `upstream.diff`, rationale and removal gates. Reproduce a patch by downloading the named `.crate` from `https://static.crates.io/crates/<name>/<name>-<version>.crate`, verifying SHA256, extracting it, and applying `patch -p1 < upstream.diff` in the extracted crate root. Root `Cargo.toml` selects these sources through repository-relative `[patch.crates-io]` entries; `Cargo.lock` fixes the graph. No Cargo cache edits or mutable branches are part of the build.

| Patch | Purpose | Removal path |
|---|---|---|
| `sqlx-sqlite-0.9.0` | One-line native SQLite bound backport, upstream `94aafe3a68884d923b0798a767c8d7f6cfda89d2` | Compatible SQLx release plus combined-link, store, migration and catalog checks |
| `msb_krun-0.1.39` | Typed process-lifetime embedded kernel provider | WI 0120 native SDK/runtime API and full boot/release checks |
| `microsandbox-filesystem-0.7.2` | Target-aware guest-agent build input | Equivalent upstream build input API |
| `microsandbox-0.7.2` | Isolated local backend builder that ignores installed Microsandbox config | Pinned upstream isolated builder plus hostile-config and native coexistence checks |
| `microsandbox-network-0.7.2` | Explicit visible-SNI enforcement without interception; exact DNS binding for suffix rules | Equivalent pinned upstream option plus SDK-boundary and native guest network tests |
| `microsandbox-types-0.7.2` | Preserve `strict_sni` through local/cloud NetworkSpec conversions | Remove with network patch after configuration roundtrip and native tests |

The six carried crate patches include the paired network/types changes. Their published archive hashes are `be4f1c9d36c1957b35d674a993a59c5db1741f47c96c952b491b09d9674b92d7` and `8f705cfd6b163fc5b00987c6cf6143b5ceed311b097e68926822b2d6daa4cfee`; their patch hashes are `993d00b0d99e436e2cebc72a8a5ccbadcfb458b8ba1d9ad4cb242bafdcc2e100` and `30bf86e5a7e58acbe889d3d05eb898795b3078dedc608d0435b252196c6eed4f`, respectively. Both are Apache-2.0. See [network provenance and boundary](microsandbox-network-0.7.2/PATCH.md) and [types provenance](microsandbox-types-0.7.2/PATCH.md).

Microsandbox source baseline is v0.7.2, revision `60d4dc8a436fb9365491567ec21d073e924e3c6d`; msb_krun originates from libkrun revision `2bd0f84ad0956f3032e0490d3b8512b6851eca12`. Kernel/guest agent release assets are pinned in `msb-payloads/manifest.toml` and fetched natively with `tools/msb-payloads/`. Linux's static cap-ng source pipeline is in `native/libcap-ng/`. These are independent of the SQLx patch's lifecycle.
