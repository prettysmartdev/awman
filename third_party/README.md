# Owned dependency patches for WI 0119

All sources come from immutable published crates. Their `PATCH.md` files record archive hashes, original licenses, exact `upstream.diff`, rationale and removal gates. Reproduce a patch by downloading the named `.crate` from `https://static.crates.io/crates/<name>/<name>-<version>.crate`, verifying SHA256, extracting it, and applying `patch -p1 < upstream.diff` in the extracted crate root. Root `Cargo.toml` selects these sources through repository-relative `[patch.crates-io]` entries; `Cargo.lock` fixes the graph. No Cargo cache edits or mutable branches are part of the build.

| Patch | Purpose | Removal path |
|---|---|---|
| `sqlx-sqlite-0.9.0` | One-line native SQLite bound backport, upstream `94aafe3a68884d923b0798a767c8d7f6cfda89d2` | Compatible SQLx release plus combined-link, store, migration and catalog checks |
| `msb_krun-0.1.39` | Typed process-lifetime embedded kernel provider | WI 0120 native SDK/runtime API and full boot/release checks |
| `microsandbox-filesystem-0.7.2` | Target-aware guest-agent build input | Equivalent upstream build input API |

Microsandbox source baseline is v0.7.2, revision `60d4dc8a436fb9365491567ec21d073e924e3c6d`; msb_krun originates from libkrun revision `2bd0f84ad0956f3032e0490d3b8512b6851eca12`. Kernel/guest agent release assets are pinned in `msb-payloads/manifest.toml` and fetched natively with `tools/msb-payloads/`. Linux's static cap-ng source pipeline is in `native/libcap-ng/`. These are independent of the SQLx patch's lifecycle.
