//! The canonical `aspec/` tarball's address.
//!
//! The download and extraction moved to `engine::aspec::AspecDownloader` in
//! WI 0114 F-29 (decision Q1: no network in Layer 0). The URL stays here: it
//! is a fixed external address, in the same category as the on-disk paths
//! beside it, and nothing about fetching it is decided in Layer 0.

/// URL for downloading the aspec repo tarball.
pub const ASPEC_TARBALL_URL: &str =
    "https://api.github.com/repos/prettysmartdev/aspec/tarball/main";
