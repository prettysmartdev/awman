//! Validated labels and query selectors used by API sessions and commands.

use std::collections::BTreeMap;

pub const MAX_LABELS: usize = 16;
/// Maximum UTF-8 byte length for each label key and value.
pub const MAX_LABEL_BYTES: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct Labels(BTreeMap<String, String>);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error("too many labels: {count} (max {max})", max = MAX_LABELS)]
    TooMany { count: usize },
    #[error("label key must not be empty")]
    KeyEmpty,
    #[error("label key '{key}' is {bytes} bytes (max {max})", max = MAX_LABEL_BYTES)]
    KeyTooLong { key: String, bytes: usize },
    #[error("label value for '{key}' is {bytes} bytes (max {max})", max = MAX_LABEL_BYTES)]
    ValueTooLong { key: String, bytes: usize },
    #[error("label key '{key}' must not contain '='")]
    KeyContainsEquals { key: String },
    #[error("label key or value for '{key}' contains a control character")]
    ControlCharacter { key: String },
    #[error("label keys are <namespace>/<name> in lowercase [a-z0-9.-]")]
    KeyFormat { key: String },
    #[error("label namespace `awman/` is reserved")]
    ReservedNamespace { key: String },
}

impl Labels {
    pub fn new(map: BTreeMap<String, String>) -> Result<Self, LabelError> {
        if map.len() > MAX_LABELS {
            return Err(LabelError::TooMany { count: map.len() });
        }
        for (key, value) in &map {
            validate_key(key)?;
            validate_value(key, value)?;
        }
        Ok(Self(map))
    }

    pub fn empty() -> Self {
        Self(BTreeMap::new())
    }
    /// Construct a view of persisted labels without applying today's write
    /// rules, preserving values written by older awman versions.
    pub fn from_stored(map: BTreeMap<String, String>) -> Self {
        Self(map)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }
}

impl TryFrom<BTreeMap<String, String>> for Labels {
    type Error = LabelError;
    fn try_from(value: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> serde::Deserialize<'de> for Labels {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = <BTreeMap<String, String> as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(map).map_err(serde::de::Error::custom)
    }
}

fn validate_key(key: &str) -> Result<(), LabelError> {
    if key.is_empty() {
        return Err(LabelError::KeyEmpty);
    }
    if key.len() > MAX_LABEL_BYTES {
        return Err(LabelError::KeyTooLong {
            key: key.into(),
            bytes: key.len(),
        });
    }
    if key.contains('=') {
        return Err(LabelError::KeyContainsEquals { key: key.into() });
    }
    if key.chars().any(char::is_control) {
        return Err(LabelError::ControlCharacter { key: key.into() });
    }
    let mut parts = key.split('/');
    let namespace = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    let allowed = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
    };
    if parts.next().is_some() || !allowed(namespace) || !allowed(name) {
        return Err(LabelError::KeyFormat { key: key.into() });
    }
    if namespace == "awman" {
        return Err(LabelError::ReservedNamespace { key: key.into() });
    }
    Ok(())
}

fn validate_value(key: &str, value: &str) -> Result<(), LabelError> {
    if value.len() > MAX_LABEL_BYTES {
        return Err(LabelError::ValueTooLong {
            key: key.into(),
            bytes: value.len(),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(LabelError::ControlCharacter { key: key.into() });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelSelector {
    Equals { key: String, value: String },
    Exists { key: String },
}

impl LabelSelector {
    pub fn parse(term: &str) -> Result<Self, LabelError> {
        Self::parse_with(term, validate_key)
    }

    /// Query parsing intentionally accepts legacy key spellings so stored
    /// dotted labels remain filterable after the write contract tightens.
    pub fn parse_query(term: &str) -> Result<Self, LabelError> {
        Self::parse_with(term, validate_query_key)
    }

    fn parse_with(
        term: &str,
        validate: fn(&str) -> Result<(), LabelError>,
    ) -> Result<Self, LabelError> {
        match term.split_once('=') {
            Some((key, value)) => {
                validate(key)?;
                validate_value(key, value)?;
                Ok(Self::Equals {
                    key: key.into(),
                    value: value.into(),
                })
            }
            None => {
                validate(term)?;
                Ok(Self::Exists { key: term.into() })
            }
        }
    }
}

fn validate_query_key(key: &str) -> Result<(), LabelError> {
    if key.is_empty() {
        return Err(LabelError::KeyEmpty);
    }
    if key.len() > MAX_LABEL_BYTES {
        return Err(LabelError::KeyTooLong {
            key: key.into(),
            bytes: key.len(),
        });
    }
    if key.contains('=') {
        return Err(LabelError::KeyContainsEquals { key: key.into() });
    }
    if key.chars().any(char::is_control) {
        return Err(LabelError::ControlCharacter { key: key.into() });
    }
    Ok(())
}
