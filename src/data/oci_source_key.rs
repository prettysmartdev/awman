//! `OciSourceKey` — the text an acquired image is cached under.
//!
//! The image cache hashes this text into the file name of its reference
//! record, so a key written under an earlier source-kind spelling
//! (`docker-store`, `apple-store`) lives at a different path than the same
//! key under the canonical spelling. New records are always keyed with the
//! canonical spelling; [`OciSourceKey::lookup_keys`] also yields the exact
//! legacy spellings, so records written before the rename stay reachable and
//! are never re-acquired just because the name changed.
//!
//! Pure data: no I/O, no hashing, and no normalization of the locator,
//! reference or platform. The engine canonicalizes registry references
//! before building a key.

use crate::data::config::image_source::ImageSourceKind;
use crate::data::oci_identity::OciPlatform;

/// Format version on the first line of every key.
const KEY_VERSION: &str = "v1";

/// What an acquired image is cached under: source kind, non-secret source
/// locator, reference, and platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciSourceKey {
    pub kind: ImageSourceKind,
    /// Non-secret location of the source (registry URL, Docker endpoint,
    /// archive path, …).
    pub locator: String,
    pub reference: String,
    pub platform: OciPlatform,
}

impl OciSourceKey {
    /// The key new records are written under, using the canonical kind
    /// spelling: `v1\n{kind}\n{locator}\n{reference}\n{platform}`.
    pub fn canonical_key(&self) -> String {
        self.key_with_kind(self.kind.as_str())
    }

    /// Every key a record for this source may have been written under:
    /// the canonical key first, then one per legacy kind spelling. No
    /// duplicates, deterministic order.
    pub fn lookup_keys(&self) -> Vec<String> {
        std::iter::once(self.kind.as_str())
            .chain(self.kind.legacy_names().iter().copied())
            .map(|kind| self.key_with_kind(kind))
            .collect()
    }

    fn key_with_kind(&self, kind: &str) -> String {
        format!(
            "{KEY_VERSION}\n{kind}\n{}\n{}\n{}",
            self.locator, self.reference, self.platform
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_spellings_keep_the_original_cache_key_text() {
        let key = OciSourceKey {
            kind: ImageSourceKind::DockerStore,
            locator: "unix:///run/docker.sock".into(),
            reference: "agent:latest".into(),
            platform: OciPlatform::host_linux(),
        };
        assert_eq!(
            key.canonical_key(),
            format!(
                "v1\ndocker-local\nunix:///run/docker.sock\nagent:latest\n{}",
                key.platform
            )
        );
        assert!(key.lookup_keys().contains(&format!(
            "v1\ndocker-store\nunix:///run/docker.sock\nagent:latest\n{}",
            key.platform
        )));
    }
}
