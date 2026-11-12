//! Validated integration metadata shared by terminal and frontend protocols.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub const MAX_ANNOTATION_KEY_BYTES: usize = 63;
pub const MAX_ANNOTATION_VALUE_BYTES: usize = 256;
pub const MAX_ANNOTATIONS: usize = 32; // was 16 in data/terminal.rs; 0142 owns the limit
pub const AWMAN_NAMESPACE: &str = "awman"; // reserved (0139 §1)

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnnotationError {
    #[error(
        "invalid annotation key `{0}`: expected <namespace>/<name> in [a-z0-9.-], at most 63 bytes"
    )]
    InvalidKey(String),
    #[error("annotation value for `{0}` is too long (max {MAX_ANNOTATION_VALUE_BYTES} bytes)")]
    ValueTooLong(String),
    #[error("annotation value for `{0}` contains control characters")]
    InvalidValue(String),
    #[error("too many annotations (max {MAX_ANNOTATIONS})")]
    TooMany,
    #[error("annotation `{key}` is claimed by both `{first}` and `{second}`")]
    NamespaceClash {
        key: String,
        first: String,
        second: String,
    },
    #[error("`{key}` is outside the `{namespace}/` namespace this presenter owns")]
    ForeignNamespace { key: String, namespace: String },
    #[error("annotation filter `{0}` is not k=v")]
    InvalidFilter(String),
    /// The object an annotation write names does not exist (or, for a
    /// presenter, no instance is in scope).
    #[error("annotation target {0} does not exist")]
    NoTarget(String),
}
impl AnnotationError {
    /// The API error code for every variant (0142 §4): `invalid_annotation`.
    pub const CODE: &'static str = "invalid_annotation";
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<String, String>",
    into = "BTreeMap<String, String>"
)]
pub struct Annotations(BTreeMap<String, String>);

impl Annotations {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn try_from_pairs<I, K, V>(pairs: I) -> Result<Self, AnnotationError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut result = Self::new();
        for (key, value) in pairs {
            result.insert(key.as_ref(), value.as_ref())?;
        }
        Ok(result)
    }
    pub fn parse_cli<S: AsRef<str>>(pairs: &[S]) -> Result<Self, AnnotationError> {
        let mut result = Self::new();
        for pair in pairs {
            let pair = pair.as_ref();
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| AnnotationError::InvalidKey(pair.into()))?;
            result.insert(key, value)?;
        }
        Ok(result)
    }
    pub fn insert(&mut self, key: &str, value: &str) -> Result<Option<String>, AnnotationError> {
        validate_annotation(key, value)?;
        if self.len() == MAX_ANNOTATIONS && !self.0.contains_key(key) {
            return Err(AnnotationError::TooMany);
        }
        Ok(self.0.insert(key.into(), value.into()))
    }
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.0.remove(key)
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }
    pub fn namespace_of(key: &str) -> Option<&str> {
        key.split_once('/')
            .map(|(ns, _)| ns)
            .filter(|ns| !ns.is_empty())
    }
    pub fn namespaces(&self) -> BTreeSet<&str> {
        self.0
            .keys()
            .filter_map(|k| Self::namespace_of(k))
            .collect()
    }
    pub fn has_namespace(&self, namespace: &str) -> bool {
        self.namespaces().contains(namespace)
    }
    pub fn in_namespace(&self, namespace: &str) -> Self {
        Self(
            self.0
                .iter()
                .filter(|(k, _)| Self::namespace_of(k) == Some(namespace))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }
    /// Merge atomically, rejecting a duplicate key even when its value agrees.
    pub fn merge_disjoint(
        &mut self,
        other: Self,
        other_owner: &str,
        self_owner: &str,
    ) -> Result<(), AnnotationError> {
        for key in other.0.keys() {
            if self.0.contains_key(key) {
                return Err(AnnotationError::NamespaceClash {
                    key: key.clone(),
                    first: self_owner.into(),
                    second: other_owner.into(),
                });
            }
        }
        if self.len() + other.len() > MAX_ANNOTATIONS {
            return Err(AnnotationError::TooMany);
        }
        self.0.extend(other.0);
        Ok(())
    }
    pub fn matches(&self, filter: &AnnotationFilter) -> bool {
        filter
            .0
            .iter()
            .all(|(k, v)| self.get(k) == Some(v.as_str()))
    }
    /// The caller supplies its registered namespace; no foreign key can enter through this helper.
    /// Validation and the entry limit are checked before replacing any existing metadata.
    pub fn replace_namespace(
        &mut self,
        namespace: &str,
        patch: &Self,
    ) -> Result<(), AnnotationError> {
        validate_annotation(&format!("{namespace}/key"), "")?;
        for key in patch.0.keys() {
            if Self::namespace_of(key) != Some(namespace) {
                return Err(AnnotationError::ForeignNamespace {
                    key: key.clone(),
                    namespace: namespace.into(),
                });
            }
        }
        let mut updated = self.clone();
        updated
            .0
            .retain(|key, _| Self::namespace_of(key) != Some(namespace));
        updated.merge_disjoint(patch.clone(), namespace, namespace)?;
        *self = updated;
        Ok(())
    }
}
impl TryFrom<BTreeMap<String, String>> for Annotations {
    type Error = AnnotationError;
    fn try_from(map: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        Self::try_from_pairs(map)
    }
}
impl From<Annotations> for BTreeMap<String, String> {
    fn from(value: Annotations) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnotationFilter(Vec<(String, String)>);
impl AnnotationFilter {
    pub fn parse<S: AsRef<str>>(values: &[S]) -> Result<Self, AnnotationError> {
        let mut pairs = Vec::new();
        for value in values {
            let value = value.as_ref();
            let (key, value) = value
                .split_once('=')
                .ok_or_else(|| AnnotationError::InvalidFilter(value.into()))?;
            validate_annotation(key, value)?;
            pairs.push((key.into(), value.into()));
            if pairs.len() > MAX_ANNOTATIONS {
                return Err(AnnotationError::TooMany);
            }
        }
        Ok(Self(pairs))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }
}

pub fn validate_annotation(key: &str, value: &str) -> Result<(), AnnotationError> {
    let valid_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
    };
    let mut parts = key.split('/');
    if key.len() > MAX_ANNOTATION_KEY_BYTES
        || !parts.next().is_some_and(valid_part)
        || !parts.next().is_some_and(valid_part)
        || parts.next().is_some()
    {
        return Err(AnnotationError::InvalidKey(key.into()));
    }
    if value.len() > MAX_ANNOTATION_VALUE_BYTES {
        return Err(AnnotationError::ValueTooLong(key.into()));
    }
    if value.chars().any(char::is_control) {
        return Err(AnnotationError::InvalidValue(key.into()));
    }
    Ok(())
}
pub const ANNOTATION_HEADER_PREFIX: &str = "x-awman-annotation-";

/// The `(header name, header value)` that carry a validated annotation.
pub fn annotation_header(key: &str, value: &str) -> (String, String) {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_graphic() && byte != b'%' {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    (
        format!("{ANNOTATION_HEADER_PREFIX}{}", key.replace('/', "~")),
        encoded,
    )
}

/// Decode one annotation header back into its validated logical key and
/// value. `name` is the full (lower-case) header name; `Ok(None)` when it
/// is not an annotation header.
pub fn annotation_from_header(
    name: &str,
    value: &[u8],
) -> Result<Option<(String, String)>, AnnotationError> {
    let Some(encoded_key) = name.strip_prefix(ANNOTATION_HEADER_PREFIX) else {
        return Ok(None);
    };
    if encoded_key.contains('/') {
        return Err(AnnotationError::InvalidKey(encoded_key.to_owned()));
    }
    let key = encoded_key.replace('~', "/");
    let invalid = || AnnotationError::InvalidValue(key.clone());
    let mut bytes = Vec::with_capacity(value.len());
    let mut iter = value.iter();
    while let Some(&byte) = iter.next() {
        if byte == b'%' {
            let hex = [
                *iter.next().ok_or_else(invalid)?,
                *iter.next().ok_or_else(invalid)?,
            ];
            let hex = std::str::from_utf8(&hex).map_err(|_| invalid())?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
        } else if byte.is_ascii_graphic() {
            bytes.push(byte);
        } else {
            return Err(invalid());
        }
    }
    let value = String::from_utf8(bytes).map_err(|_| invalid())?;
    validate_annotation(&key, &value)?;
    Ok(Some((key, value)))
}
