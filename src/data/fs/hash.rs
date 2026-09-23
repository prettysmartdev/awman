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
pub fn sha256_hex(data: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_path_separators_and_spaces() {
        assert_eq!(
            sanitize_name_for_filename("issue-triage across Rust/TS/Python"),
            "issue-triage-across-Rust-TS-Python"
        );
    }

    #[test]
    fn sanitize_keeps_alphanumerics_dash_and_underscore() {
        assert_eq!(
            sanitize_name_for_filename("build_and-test-01"),
            "build_and-test-01"
        );
    }

    #[test]
    fn sanitize_truncates_to_64_characters() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_name_for_filename(&long).len(), 64);
    }

    #[test]
    fn sanitize_never_returns_an_empty_component() {
        // Every character is replaced, then the result would be all dashes —
        // still non-empty — but an empty input must not produce an empty name.
        assert_eq!(sanitize_name_for_filename(""), "workflow");
    }

    #[test]
    fn sha256_hex_is_lowercase_hex_of_the_right_length() {
        let h = sha256_hex("awman");
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn sha256_hex_is_stable() {
        // Pinned: the first 8 characters of this digest appear in every
        // workflow-state filename and every image tag, so a change here
        // orphans state files and rebuilds images.
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
