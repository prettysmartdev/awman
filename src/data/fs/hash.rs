//! Filename and digest helpers shared by the Layer 0 stores.
//!
//! Both live here rather than beside any one store because three unrelated
//! callers need them and none of them owns the others: the workflow-state
//! store (`<repo-hash>-<work-item>-<name>.json`), the image tagger
//! (`src/data/image_tags.rs`), and the container attach socket
//! (`src/engine/container/attach_socket.rs`). Before WI 0114 F-30 they lived
//! on a second, otherwise-dead `WorkflowStateStore`, which is why that store
//! outlived its own purpose.

/// Sanitize a workflow name/title into a filesystem-safe filename component.
///
/// A workflow's name is derived from its human-readable title, which may
/// legitimately contain path separators (`/`), spaces, or other characters
/// that are unsafe in a filename — e.g. a dynamic leader emitting the title
/// `"issue-triage across Rust/TS/Python"`. Embedding such a name directly in a
/// state filename turns the `/` into directory separators and makes
/// `std::fs::write` fail with `No such file or directory`. Only ASCII
/// alphanumerics, `-`, and `_` survive; every other character becomes `-`.
pub fn sanitize_name_for_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(64);
    if out.is_empty() {
        out.push_str("workflow");
    }
    out
}

/// Compute the SHA-256 hash of `data`, returned as a lowercase hex string.
///
/// Accepts text (`&str`, `&String`) and raw bytes (`&[u8]`, `&Vec<u8>`) alike,
/// so the stores (filename hashes) and the OCI image layer (blob digests)
/// share one implementation.
pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(data.as_ref());
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}
