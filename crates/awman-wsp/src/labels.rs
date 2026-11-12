//! Label provenance is retained so reported facts cannot become identity claims.
use std::collections::BTreeMap;
pub const RESERVED_PREFIX: &str = "awman/";
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error("label keys must be lowercase names or namespace/name using a-z, 0-9, dot and dash")]
    InvalidKey,
    #[error("configured labels cannot use the awman/ namespace")]
    Reserved,
    #[error("configured labels conflict with server-owned keys")]
    ServerOwned,
}
pub fn validate_key(key: &str) -> Result<(), LabelError> {
    let parts: Vec<_> = key.split('/').collect();
    if (parts.len() == 1 && key.contains('.'))
        || parts.len() > 2
        || parts.iter().any(|s| {
            s.is_empty()
                || s.len() > 63
                || !s.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-')
                })
        })
    {
        return Err(LabelError::InvalidKey);
    }
    Ok(())
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerLabels {
    pub reported: BTreeMap<String, String>,
    pub configured: BTreeMap<String, String>,
    pub server: BTreeMap<String, String>,
}
impl WorkerLabels {
    pub fn new(
        reported: BTreeMap<String, String>,
        configured: BTreeMap<String, String>,
    ) -> Result<Self, LabelError> {
        for key in reported.keys().chain(configured.keys()) {
            validate_key(key)?;
        }
        if configured.keys().any(|k| k.starts_with(RESERVED_PREFIX)) {
            return Err(LabelError::Reserved);
        }
        Ok(Self {
            reported,
            configured,
            server: BTreeMap::new(),
        })
    }
    /// Refuse worker identity claims from either source instead of silently overriding them.
    pub fn with_server(mut self, server: BTreeMap<String, String>) -> Result<Self, LabelError> {
        for key in server.keys() {
            validate_key(key)?;
        }
        if self
            .configured
            .keys()
            .chain(self.reported.keys())
            .any(|key| server.contains_key(key))
        {
            return Err(LabelError::ServerOwned);
        }
        self.server = server;
        Ok(self)
    }
    pub fn merged(&self) -> BTreeMap<String, String> {
        let mut labels = self.configured.clone();
        labels.extend(self.reported.clone());
        labels.extend(self.server.clone());
        labels
    }
}
