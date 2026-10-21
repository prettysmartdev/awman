# Embedded runtime notices and source obligations

The embedded Linux kernel and guest agent come from Microsandbox v0.7.8
(`7b7b9dc89e9a1c77801918f7833c3381d1754579`). Its libkrun revision is
`b35c7e118d7a1e48c8fad103b258e64ff5c49147`; the release's libkrunfw submodule is
`21f169f7e94798c8916315f8ac6f5999d5b88566` (Linux 6.12.111).
The kernel is GPL-2.0. Distributors must provide the corresponding source,
kernel configuration and any required source offer for this exact kernel.
The release inputs, checksums and extraction process are recorded in
[manifest.toml](third_party/msb-payloads/manifest.toml) and
[the payload tools](tools/msb-payloads/). Source is identified by the immutable
[Microsandbox revision](https://github.com/superradcompany/microsandbox/tree/7b7b9dc89e9a1c77801918f7833c3381d1754579)
and [libkrunfw revision](https://github.com/superradcompany/libkrunfw/tree/21f169f7e94798c8916315f8ac6f5999d5b88566).
Unverified target records are not release acceptance evidence.

Five carried crate patches remain, listed in [third_party/README.md](third_party/README.md):
[SDK](third_party/microsandbox-0.7.8/PATCH.md),
[filesystem](third_party/microsandbox-filesystem-0.7.8/PATCH.md),
[network](third_party/microsandbox-network-0.7.8/PATCH.md),
[types](third_party/microsandbox-types-0.7.8/PATCH.md) (all Apache-2.0), and
[SQLx SQLite](third_party/sqlx-sqlite-0.9.0/PATCH.md) (MIT OR Apache-2.0).
Their original license texts are retained in those directories.
`msb_krun =0.1.41` is an unmodified Apache-2.0 registry dependency with
`static-krunfw`, not a carried patch.

Current Linux builds use upstream's pure Rust `caps` capability implementation;
they do not link libcap-ng. The historical libcap-ng source pipeline is retained
only for the standalone 0.7.2 compatibility probe. It is not a distribution input
or a prerequisite of current builtin builds.

The executable also uses ordinary host ABI libraries and macOS system frameworks.
