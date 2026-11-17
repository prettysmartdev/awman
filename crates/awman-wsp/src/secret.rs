//! Shared secret values. Only explicit authenticated wire adapters serialize values.
use std::collections::BTreeMap;
use std::fmt;

pub const MAX_COMMAND_SECRETS: usize = 32;
pub const MAX_COMMAND_SECRET_BYTES: usize = 8 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn input_buffer() -> Self {
        Self(String::new())
    }
    pub fn push_char(&mut self, ch: char) {
        self.0.push(ch);
    }
    pub fn pop_char(&mut self) {
        self.0.pop();
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.resize(bytes.capacity(), 0);
        bytes.fill(0);
        // Make the cleared allocation observable before it is freed.
        std::hint::black_box(&mut bytes);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretName(String);

impl SecretName {
    pub fn parse(name: &str) -> Result<Self, CommandSecretsInvalid> {
        let mut bytes = name.bytes();
        if name.len() > 128
            || !bytes
                .next()
                .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            || !bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(CommandSecretsInvalid::BadName { name: name.into() });
        }
        Ok(Self(name.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommandSecretsInvalid {
    #[error("too many command secrets (maximum 32)")]
    TooMany { count: usize },
    #[error("command secret '{name}' exceeds 8 KiB")]
    TooLarge { name: String },
    #[error("command secret '{name}' is empty")]
    Empty { name: String },
    #[error("command secret name '{name}' must match ^[A-Z_][A-Z0-9_]*$ (maximum 128 bytes)")]
    BadName { name: String },
    #[error("command secret '{name}' contains NUL")]
    Nul { name: String },
    #[error("duplicate command secret '{name}'")]
    Duplicate { name: String },
}

#[derive(Default)]
pub struct CommandSecrets(BTreeMap<SecretName, SecretString>);

impl CommandSecrets {
    pub fn new(
        entries: impl IntoIterator<Item = (String, SecretString)>,
    ) -> Result<Self, CommandSecretsInvalid> {
        let mut secrets = BTreeMap::new();
        for (name, value) in entries {
            if secrets.len() >= MAX_COMMAND_SECRETS {
                return Err(CommandSecretsInvalid::TooMany {
                    count: secrets.len() + 1,
                });
            }
            let key = SecretName::parse(&name)?;
            if value.expose().is_empty() {
                return Err(CommandSecretsInvalid::Empty { name });
            }
            if value.expose().len() > MAX_COMMAND_SECRET_BYTES {
                return Err(CommandSecretsInvalid::TooLarge { name });
            }
            if value.expose().contains('\0') {
                return Err(CommandSecretsInvalid::Nul { name });
            }
            if secrets.contains_key(&key) {
                return Err(CommandSecretsInvalid::Duplicate { name });
            }
            secrets.insert(key, value);
        }
        Ok(Self(secrets))
    }
    pub fn get(&self, name: &str) -> Option<&SecretString> {
        self.0.get(&SecretName::parse(name).ok()?)
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(SecretName::as_str)
    }
    pub fn values(&self) -> impl Iterator<Item = &SecretString> {
        self.0.values()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SecretString)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for CommandSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CommandSecrets")
            .field(&self.names().collect::<Vec<_>>())
            .finish()
    }
}

/// Server-only serialization: ordinary WorkItem/Lease serde intentionally omits values.
pub mod server_wire {
    use serde_json::Value;

    pub fn work_item(item: &crate::WorkItem) -> Value {
        let mut value = serde_json::to_value(item).expect("work item serialization");
        if let Some(secrets) = item.exec.secrets.shared() {
            value["exec"]["secrets"] = Value::Object(
                secrets
                    .iter()
                    .map(|(name, value)| {
                        (name.to_owned(), Value::String(value.expose().to_owned()))
                    })
                    .collect(),
            );
        }
        value
    }

    pub fn lease(lease: &crate::Lease) -> Value {
        let mut value = serde_json::to_value(lease).expect("lease serialization");
        value["item"] = work_item(&lease.item);
        value
    }
}
