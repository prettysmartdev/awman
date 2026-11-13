//! Caller-chosen names for durable repository context.
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ContextKey(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("context key must match ^[a-z0-9][a-z0-9._-]{{0,127}}$")]
pub struct ContextKeyError;

impl ContextKey {
    pub const MAX_LEN: usize = 128;
    pub fn parse(raw: &str) -> Result<Self, ContextKeyError> {
        let first = raw.as_bytes().first().copied().ok_or(ContextKeyError)?;
        if raw.len() > Self::MAX_LEN
            || !(first.is_ascii_lowercase() || first.is_ascii_digit())
            || !raw
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(ContextKeyError);
        }
        Ok(Self(raw.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn dir_name(&self) -> String {
        #[cfg(windows)]
        {
            windows_dir_name(&self.0)
        }
        #[cfg(not(windows))]
        {
            self.0.clone()
        }
    }
}
#[cfg(any(windows, test))]
fn windows_dir_name(key: &str) -> String {
    if key.ends_with('.') || super::path::reserved_name(key) {
        format!("%{key}%")
    } else {
        key.to_owned()
    }
}
impl<'de> Deserialize<'de> for ContextKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_directory_encoding_is_bounded_and_disjoint() {
        let keys = [
            format!("{}.", "a".repeat(127)),
            format!("con.{}", "x".repeat(124)),
            "con".into(),
            "a.".into(),
            "a".into(),
            "nul".into(),
        ];
        let names: std::collections::BTreeSet<_> = keys
            .iter()
            .map(|key| {
                ContextKey::parse(key).unwrap();
                let name = windows_dir_name(key);
                assert!(name.len() <= 130);
                assert!(!name.ends_with('.'));
                assert!(!super::super::path::reserved_name(&name));
                name
            })
            .collect();
        assert_eq!(names.len(), keys.len());
    }
    #[test]
    fn key_regex_and_serde() {
        for key in ["a", "0", "my-key", "a._-9", "con", "a.", &"x".repeat(128)] {
            assert_eq!(ContextKey::parse(key).unwrap().as_str(), key);
        }
        for key in [
            "",
            "_a",
            ".a",
            "-a",
            "A",
            "a/b",
            "a\\b",
            "a:b",
            "a b",
            "é",
            &"x".repeat(129),
        ] {
            assert!(ContextKey::parse(key).is_err(), "{key}");
            assert!(serde_json::from_value::<ContextKey>(serde_json::json!(key)).is_err());
        }
    }
}
