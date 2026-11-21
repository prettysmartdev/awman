//! Named settings profiles and agent-owned conversation ids — Layer 0.
//!
//! `--settings-profile <NAME>` selects a persistent, private provider
//! settings/history store for one agent in one repository. `--agent-session
//! <ID>` names an exact conversation the *agent* owns inside that profile.
//! Both arrive as caller input, so each has one validated type here: a
//! profile name is a name, never a path, and a native session id is an
//! opaque argv token, never shell text or a path.
//!
//! The profile layout, manifest and lease mechanics live with `DataPaths`
//! (`data::fs`); this module only holds the shared names and states.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::data::config::fields::is_valid_map_name;

/// The catalogue flag (`--settings-profile`) and the workflow step key
/// (`settings_profile`).
pub const SETTINGS_PROFILE_FLAG: &str = "settings-profile";
pub const SETTINGS_PROFILE_STEP_KEY: &str = "settings_profile";

/// The catalogue flag (`--agent-session`) and its `flags_applied` key.
pub const AGENT_SESSION_FLAG: &str = "agent-session";
pub const AGENT_SESSION_APPLIED_KEY: &str = "agent_session";

/// The directory under a repository's state dir that holds its profiles:
/// `<data_home>/data/repos/<repo-key>/agent-profiles/<agent>/<NAME>/`.
pub const AGENT_PROFILES_DIR: &str = "agent-profiles";

/// The profile manifest's schema version.
pub const PROFILE_MANIFEST_SCHEMA: u32 = 1;

/// `/v1/status` capabilities for profiles and native resume.
pub const SETTINGS_PROFILES_CAPABILITY: &str = "settings_profiles";
pub const NATIVE_SESSION_RESUME_CAPABILITY: &str = "native_session_resume";

/// Why a caller-supplied profile name or native session id was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileInputError {
    #[error(
        "invalid settings profile name: expected [a-z0-9][a-z0-9_-]{{0,62}} (a name, not a path)"
    )]
    InvalidProfileName,
    #[error("invalid agent session id: expected 1..=512 bytes with no control characters")]
    InvalidNativeSessionId,
}

/// A validated settings-profile name: `[a-z0-9][a-z0-9_-]{0,62}`, the same
/// contract as every other config map name. It is safe as one path segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SettingsProfileName(String);

impl SettingsProfileName {
    pub fn parse(name: &str) -> Result<Self, ProfileInputError> {
        if is_valid_map_name(name)
            && !matches!(
                name,
                "con"
                    | "prn"
                    | "aux"
                    | "nul"
                    | "com1"
                    | "com2"
                    | "com3"
                    | "com4"
                    | "com5"
                    | "com6"
                    | "com7"
                    | "com8"
                    | "com9"
                    | "lpt1"
                    | "lpt2"
                    | "lpt3"
                    | "lpt4"
                    | "lpt5"
                    | "lpt6"
                    | "lpt7"
                    | "lpt8"
                    | "lpt9"
            )
        {
            Ok(Self(name.to_owned()))
        } else {
            Err(ProfileInputError::InvalidProfileName)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SettingsProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SettingsProfileName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SettingsProfileName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// An agent-owned conversation id (Claude `session_id`, Codex `thread_id`,
/// …): opaque, 1..=512 UTF-8 bytes, no control characters.
///
/// It is not an awman id. It is only ever compared for exact equality and
/// passed to the provider as one argv token. `Debug` does not print it in
/// full so a stray log line cannot leak a whole id.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NativeSessionId(String);

impl NativeSessionId {
    pub const MAX_BYTES: usize = 512;

    pub fn parse(id: &str) -> Result<Self, ProfileInputError> {
        let valid =
            !id.is_empty() && id.len() <= Self::MAX_BYTES && !id.chars().any(char::is_control);
        if valid {
            Ok(Self(id.to_owned()))
        } else {
            Err(ProfileInputError::InvalidNativeSessionId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for NativeSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NativeSessionId({} bytes)", self.0.len())
    }
}

impl fmt::Display for NativeSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for NativeSessionId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NativeSessionId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A profile's lifecycle state, as `agents profile list` reports it.
///
/// `closing` is a profile whose execution is ending and whose writers,
/// cleanup or snapshot publication are not yet confirmed complete. Only an
/// `inactive` profile may be deleted or opened by a new writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileState {
    Active,
    Inactive,
    Closing,
}

impl ProfileState {
    pub const ALL: [Self; 3] = [Self::Active, Self::Inactive, Self::Closing];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Closing => "closing",
        }
    }

    /// Whether a new writer may enter, or the profile may be deleted.
    pub fn admits_writer(self) -> bool {
        self == Self::Inactive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_follow_the_map_name_contract() {
        for ok in ["a", "0", "altana-review", "a_b-9", &"a".repeat(63)] {
            assert_eq!(SettingsProfileName::parse(ok).unwrap().as_str(), ok);
        }
        for bad in [
            "",
            "-a",
            "_a",
            "A",
            "a b",
            "a/b",
            "..",
            ".",
            "a.b",
            "a\\b",
            "é",
            &"a".repeat(64),
        ] {
            assert_eq!(
                SettingsProfileName::parse(bad),
                Err(ProfileInputError::InvalidProfileName),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn profile_names_validate_on_deserialize() {
        let name: SettingsProfileName = serde_json::from_str("\"altana-review\"").unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"altana-review\"");
        assert!(serde_json::from_str::<SettingsProfileName>("\"../x\"").is_err());
    }

    #[test]
    fn native_session_ids_are_bounded_opaque_tokens() {
        for ok in [
            "a",
            "019a3f7e-1c2d-7abc-9def-0123456789ab",
            "ses_ABC/def:1",
            "--looks-like-a-flag",
            "ünïcode",
            &"x".repeat(512),
        ] {
            assert_eq!(NativeSessionId::parse(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "a\nb", "a\0b", "a\tb", "\u{1b}[0m", &"x".repeat(513)] {
            assert_eq!(
                NativeSessionId::parse(bad),
                Err(ProfileInputError::InvalidNativeSessionId),
                "{bad:?}"
            );
        }
        // 512 is a byte bound, not a character bound.
        assert!(NativeSessionId::parse(&"é".repeat(256)).is_ok());
        assert!(NativeSessionId::parse(&"é".repeat(257)).is_err());
    }

    #[test]
    fn native_session_ids_validate_on_deserialize_and_hide_in_debug() {
        let id: NativeSessionId = serde_json::from_str("\"thread-123\"").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"thread-123\"");
        assert!(!format!("{id:?}").contains("thread-123"));
        assert!(serde_json::from_str::<NativeSessionId>("\"\"").is_err());
    }

    #[test]
    fn only_an_inactive_profile_admits_a_writer() {
        for state in ProfileState::ALL {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
            assert_eq!(state.admits_writer(), state == ProfileState::Inactive);
        }
    }
}
