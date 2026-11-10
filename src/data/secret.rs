//! Secret data with an explicit authenticated frontend serde adapter.

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
