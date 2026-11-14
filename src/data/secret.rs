//! Secret data with an explicit authenticated frontend serde adapter.

use std::collections::BTreeMap;

pub const MAX_COMMAND_SECRETS: usize = 32;
pub const MAX_COMMAND_SECRET_BYTES: usize = 8 * 1024;

/// A validated environment name. Secret values have no persistence adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretName(String);

impl SecretName {
    pub fn parse(name: &str) -> Result<Self, CommandSecretsInvalid> {
        let mut bytes = name.bytes();
        if name.len() > 128
            || !bytes
                .next()
                .is_some_and(|b| b == b'_' || b.is_ascii_uppercase())
            || !bytes.all(|b| b == b'_' || b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return Err(CommandSecretsInvalid::BadName { name: name.into() });
        }
        Ok(Self(name.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandSecretsInvalid {
    #[error("at most 32 command secrets are accepted (received {count})")]
    TooMany { count: usize },
    #[error("secret {name:?} exceeds 8192 bytes")]
    TooLarge { name: String },
    #[error("secret {name:?} must not be empty")]
    Empty { name: String },
    #[error("secret name must match ^[A-Z_][A-Z0-9_]*$ and be at most 128 bytes")]
    BadName { name: String },
    #[error("secret {name:?} must not contain NUL")]
    Nul { name: String },
    #[error("secret name {name:?} is duplicated")]
    Duplicate { name: String },
}

/// Command-lifetime values. Never Serialize or Display; Debug lists names only.
#[derive(Default)]
pub struct CommandSecrets(BTreeMap<SecretName, SecretString>);

impl CommandSecrets {
    pub fn new(
        entries: impl IntoIterator<Item = (String, SecretString)>,
    ) -> Result<Self, CommandSecretsInvalid> {
        let mut values = BTreeMap::new();
        for (name, value) in entries {
            let key = SecretName::parse(&name)?;
            if values.len() == MAX_COMMAND_SECRETS {
                return Err(CommandSecretsInvalid::TooMany {
                    count: values.len() + 1,
                });
            }
            if value.expose().is_empty() {
                return Err(CommandSecretsInvalid::Empty { name });
            }
            if value.expose().len() > MAX_COMMAND_SECRET_BYTES {
                return Err(CommandSecretsInvalid::TooLarge { name });
            }
            if value.expose().contains('\0') {
                return Err(CommandSecretsInvalid::Nul { name });
            }
            if values.insert(key, value).is_some() {
                return Err(CommandSecretsInvalid::Duplicate { name });
            }
        }
        Ok(Self(values))
    }

    pub fn get(&self, name: &str) -> Option<&SecretString> {
        self.0.get(&SecretName(name.into()))
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(SecretName::as_str)
    }
    pub fn values(&self) -> impl Iterator<Item = &SecretString> {
        self.0.values()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for CommandSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CommandSecrets")
            .field(&self.names().collect::<Vec<_>>())
            .finish()
    }
}

/// Wrapper that keeps a credential value out of logs, `Debug` output and
/// serialized payloads. Deliberately implements neither `Display` nor
/// `Serialize`; the only way to reach the bytes is [`SecretString::expose`].
/// Secrets deliberately have no blanket serde implementation.
/// ```compile_fail
/// let secret = awman::data::secret::SecretString::new("credential");
/// let _ = serde_json::to_string(&secret);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn input_buffer() -> Self {
        Self(String::with_capacity(8192))
    }

    pub fn push_char(&mut self, ch: char) {
        if self.0.len() + ch.len_utf8() <= 8192 {
            self.0.push(ch);
        }
    }

    pub fn pop_char(&mut self) {
        if let Some(ch) = self.0.chars().last() {
            let len = self.0.len() - ch.len_utf8();
            self.0.truncate(len);
            self.0.extend(std::iter::repeat_n('\0', ch.len_utf8()));
            std::hint::black_box(&mut self.0);
            self.0.truncate(len);
        }
    }

    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The only accessor. Every call site is a place a reviewer must check.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    /// Best-effort overwrite of the buffer before it is freed (no `unsafe`, no
    /// `zeroize` crate): truncate, then refill the retained capacity with NULs
    /// in place. Pushing at most `capacity` bytes never reallocates, so the
    /// writes land on the allocation that held the secret. `black_box` keeps
    /// the optimiser from discarding writes to memory about to be freed.
    fn drop(&mut self) {
        let buffer = &mut self.0;
        let capacity = buffer.capacity();
        buffer.clear();
        buffer.extend(std::iter::repeat_n('\0', capacity));
        std::hint::black_box(buffer);
    }
}

impl std::fmt::Debug for SecretString {
    /// Renders exactly `SecretString(<redacted>)` — never the value, never a
    /// prefix of the value, never its length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

/// The sole serialization path for authenticated frontend dialog answers.
pub mod frontend_wire {
    use super::SecretString;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(
        value: &SecretString,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.expose())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SecretString, D::Error> {
        String::deserialize(deserializer).map(SecretString::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_secret_limits_and_names_are_validated_without_values_in_errors() {
        for name in ["A", "_A1", "_", "TOKEN_23"] {
            assert!(SecretName::parse(name).is_ok());
        }
        for name in ["", "a", "1A", "A-B", "A B", "Ä", "A\n"] {
            assert!(SecretName::parse(name).is_err());
        }
        let entries =
            |n| (0..n).map(|i| (format!("KEY_{i}"), SecretString::new("test-secret-value")));
        assert!(CommandSecrets::new(entries(32)).is_ok());
        assert!(matches!(
            CommandSecrets::new(entries(33)),
            Err(CommandSecretsInvalid::TooMany { .. })
        ));
        assert!(CommandSecrets::new([("KEY".into(), SecretString::new("x".repeat(8192)))]).is_ok());
        for (value, expected) in [
            ("x".repeat(8193), "exceeds"),
            (String::new(), "empty"),
            ("test-secret-\0value".into(), "NUL"),
        ] {
            let error = CommandSecrets::new([("KEY".into(), SecretString::new(value.clone()))])
                .unwrap_err();
            assert!(error.to_string().contains(expected));
            if !value.is_empty() {
                assert!(!error.to_string().contains(&value));
            }
        }
        assert!(matches!(
            CommandSecrets::new(entries(2).map(|(_, value)| ("KEY".into(), value))),
            Err(CommandSecretsInvalid::Duplicate { .. })
        ));
    }

    #[test]
    fn command_secrets_debug_lists_names_only() {
        let secrets =
            CommandSecrets::new([("TOKEN".into(), SecretString::new("test-secret-debug"))])
                .unwrap();
        assert_eq!(secrets.names().collect::<Vec<_>>(), ["TOKEN"]);
        assert!(format!("{secrets:?}").contains("TOKEN"));
        assert!(!format!("{secrets:?}").contains("test-secret-debug"));
    }
}
