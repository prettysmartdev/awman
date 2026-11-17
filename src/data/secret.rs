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
