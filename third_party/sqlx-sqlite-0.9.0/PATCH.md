Published crate: sqlx-sqlite 0.9.0, SHA256 `488e99c397a62007e4229aec669a179816339afc6d2620ca6fa420dbee2e982c`. License: MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).

WI 0119 changes only the Cargo-normalized `libsqlite3-sys` upper bound from `<0.38.0` to `<0.39.0`, backporting upstream SQLx commit `94aafe3a68884d923b0798a767c8d7f6cfda89d2`. `Cargo.toml.orig` is the unmodified published source manifest. `upstream.diff` applies to the published crate with `patch -p1`.

Remove this patch when a released SQLx crate contains the bound fix and passes the combined awman/msb link, existing awman store tests, msb migrations, and old/new catalog checks in `tools/oci-runtime-spike/sqlite-resolution/` on supported targets. Do not alter awman's rusqlite baseline.
