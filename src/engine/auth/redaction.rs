//! The one redaction implementation (WI 0149 §6).
//!
//! A per-run [`RedactionSet`] collects every secret value that enters a grant
//! (env token, pasted token, `gh auth token`, `acquire` output). Layer 1 runs
//! all text that leaves a container step through [`RedactionSet::redact`]
//! before any frontend, log or failure file sees it. Clones share one set, so
//! a token registered late (a pre-teardown refresh) reaches every holder.

use std::sync::{Arc, RwLock};

use super::credential::SecretString;

#[derive(Clone, Default)]
pub struct RedactionSet {
    inner: Arc<RwLock<Vec<SecretString>>>,
}

impl RedactionSet {
    pub const MARK: &'static str = "***";
    /// Shorter secrets are not registered: replacing them would mangle
    /// ordinary output without protecting anything meaningful.
    pub const MIN_LEN: usize = 1;

    pub fn new() -> Self {
        Self::default()
    }

    /// Register `secret`. Duplicates and values shorter than [`Self::MIN_LEN`]
    /// are ignored. Kept sorted longest-first so a secret that contains
    /// another is replaced whole.
    pub fn add(&self, secret: SecretString) {
        if secret.expose().chars().count() < Self::MIN_LEN {
            return;
        }
        let mut secrets = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if secrets.iter().any(|s| s.expose() == secret.expose()) {
            return;
        }
        secrets.push(secret);
        secrets.sort_by_key(|s| std::cmp::Reverse(s.expose().len()));
    }

    /// Replace every exact occurrence of a registered secret with [`Self::MARK`].
    pub fn redact(&self, text: &str) -> String {
        let secrets = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut out = text.to_string();
        for secret in secrets.iter() {
            if out.contains(secret.expose()) {
                out = out.replace(secret.expose(), Self::MARK);
            }
        }
        out
    }

    /// Byte-level variant for raw process output that may not be UTF-8.
    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        let secrets = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut out = bytes.to_vec();
        for secret in secrets.iter() {
            out = replace_bytes(&out, secret.expose().as_bytes(), Self::MARK.as_bytes());
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

impl std::fmt::Debug for RedactionSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.inner.read().unwrap_or_else(|e| e.into_inner()).len();
        write!(f, "RedactionSet(<{n} secrets>)")
    }
}

fn replace_bytes(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_longest_first_including_short_values() {
        let set = RedactionSet::new();
        set.add(SecretString::new("abc"));
        set.add(SecretString::new("ghp_secret"));
        set.add(SecretString::new("ghp_secret_long"));
        set.add(SecretString::new("ghp_secret"));
        assert_eq!(
            set.redact("a ghp_secret_long b ghp_secret abc"),
            "a *** b *** ***"
        );
        assert_eq!(set.redact_bytes(b"x ghp_secret\xff"), b"x ***\xff".to_vec());
        assert!(!format!("{set:?}").contains("ghp"));
        assert_eq!(format!("{set:?}"), "RedactionSet(<3 secrets>)");
    }

    #[test]
    fn clones_share_one_set() {
        let set = RedactionSet::new();
        let clone = set.clone();
        assert!(clone.is_empty());
        set.add(SecretString::new("late-token"));
        assert_eq!(clone.redact("late-token"), "***");
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn exact_match_longest_first() {
        let set = RedactionSet::new();
        set.add(SecretString::new("token_abcdef"));
        set.add(SecretString::new("token_abcdef_extended"));
        assert_eq!(set.redact("x token_abcdef_extended y"), "x *** y");
        assert_eq!(
            set.redact("TOKEN_ABCDEF"),
            "TOKEN_ABCDEF",
            "case-sensitive exact match"
        );
    }

    #[test]
    fn short_secrets_redacted_at_the_boundary() {
        let set = RedactionSet::new();
        set.add(SecretString::new("12345"));
        assert!(!set.is_empty());
        set.add(SecretString::new("123456"));
        assert!(!set.is_empty());
        assert_eq!(set.redact("a 123456 b 12345"), "a *** b ***");
    }

    #[test]
    fn debug_hides_values() {
        let set = RedactionSet::new();
        set.add(SecretString::new("super-secret-value"));
        assert!(!format!("{set:?}").contains("super"));
        assert!(!format!("{set:#?}").contains("secret-value"));
    }

    #[test]
    fn multiple_occurrences_and_multiline_output_are_redacted() {
        let set = RedactionSet::new();
        set.add(SecretString::new("ghp_multi_line"));
        let text = "line1 ghp_multi_line\nline2 ghp_multi_line ghp_multi_line\n";
        assert_eq!(set.redact(text), "line1 ***\nline2 *** ***\n");
        assert_eq!(
            set.redact_bytes(b"\xffghp_multi_line\x00ghp_multi_line"),
            b"\xff***\x00***".to_vec()
        );
        // Empty input and input without secrets pass through untouched.
        assert_eq!(set.redact(""), "");
        assert_eq!(set.redact("nothing here"), "nothing here");
    }
}
