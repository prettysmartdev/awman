//! Secret data with an explicit authenticated frontend serde adapter.
//! Secret values deliberately do not implement `Serialize`.
//! ```compile_fail
//! let secret = awman::data::secret::SecretString::new("credential");
//! let _ = serde_json::to_string(&secret);
//! ```

pub use awman_wsp::secret::{
    CommandSecrets, CommandSecretsInvalid, SecretName, SecretString, MAX_COMMAND_SECRETS,
    MAX_COMMAND_SECRET_BYTES,
};

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

/// Compare transient idempotency material without secret-derived hashes.
pub fn same_bundle(a: &CommandSecrets, b: &CommandSecrets) -> bool {
    if a.names().collect::<Vec<_>>() != b.names().collect::<Vec<_>>() {
        return false;
    }
    a.iter().fold(true, |equal, (name, value)| {
        let other = b.get(name).expect("matching names");
        let bytes = value.expose().as_bytes();
        let rhs = other.expose().as_bytes();
        let mut diff = bytes.len() ^ rhs.len();
        for i in 0..bytes.len().max(rhs.len()) {
            diff |=
                usize::from(bytes.get(i).copied().unwrap_or(0) ^ rhs.get(i).copied().unwrap_or(0));
        }
        equal & (diff == 0)
    })
}
