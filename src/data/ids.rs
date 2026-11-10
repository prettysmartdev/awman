//! Opaque, resource-specific identifiers used on the API wire and in stores.

use std::{fmt, str::FromStr};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("invalid {kind} id `{value}` (expected {prefix}_<32 hex>)")]
    Invalid {
        kind: &'static str,
        prefix: &'static str,
        value: String,
    },
}

macro_rules! typed_id {
    ($name:ident, $prefix:literal, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);
        impl $name {
            pub const PREFIX: &'static str = $prefix;
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::new_v4().simple()))
            }
            pub fn parse(s: &str) -> Result<Self, IdError> {
                let valid = s.strip_prefix(concat!($prefix, "_")).is_some_and(|v| {
                    v.len() == 32
                        && v.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                });
                if valid {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(IdError::Invalid {
                        kind: $kind,
                        prefix: $prefix,
                        value: s.to_owned(),
                    })
                }
            }
            pub fn from_legacy_uuid(s: &str) -> Option<Self> {
                let compact: String = s.chars().filter(|&c| c != '-').collect();
                if s.len() == 36
                    && s.as_bytes().get(8) == Some(&b'-')
                    && s.as_bytes().get(13) == Some(&b'-')
                    && s.as_bytes().get(18) == Some(&b'-')
                    && s.as_bytes().get(23) == Some(&b'-')
                    && compact.len() == 32
                    && compact
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    Some(Self(format!("{}_{}", $prefix, compact)))
                } else {
                    None
                }
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::parse(&s).map_err(serde::de::Error::custom)
            }
        }
        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::Borrowed(ValueRef::Text(
                    self.as_str().as_bytes(),
                )))
            }
        }
        impl FromSql for $name {
            fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
                let s = v.as_str()?;
                Self::parse(s).map_err(|_| FromSqlError::InvalidType)
            }
        }
    };
}

typed_id!(LaunchContextId, "lc", "launch context");
typed_id!(WorkerSessionId, "ses", "worker session");
typed_id!(CommandId, "cmd", "command");
typed_id!(TaskId, "task", "squad task");
typed_id!(RunId, "run", "squad run");
typed_id!(GrantId, "grant", "prerequisite grant");
typed_id!(FrontendId, "fe", "frontend");
typed_id!(DialogId, "dlg", "dialog");
typed_id!(InstanceId, "ins", "engine instance");
typed_id!(TabId, "tab", "engine tab");
typed_id!(TerminalId, "term", "terminal");
typed_id!(LeaseId, "lease", "work lease");
typed_id!(KeyId, "key", "API key");
typed_id!(BrowserLinkId, "blk", "browser link");
typed_id!(BrowserGrantId, "brw", "browser grant");

pub const ID_PREFIXES: &[(&str, &str)] = &[
    ("ses", "worker session"),
    ("cmd", "command"),
    ("task", "squad task"),
    ("run", "squad run"),
    ("grant", "prerequisite grant"),
    ("ins", "engine instance"),
    ("fe", "frontend"),
    ("dlg", "dialog"),
    ("tab", "engine tab"),
    ("term", "terminal"),
    ("lease", "work lease"),
    ("key", "API key"),
    ("blk", "browser link"),
    ("brw", "browser grant"),
    // Upgrade tickets are opaque `awt_<43 base64url>` secrets, not typed ids;
    // the prefix is reserved so no resource id can collide with it.
    ("awt", "browser upgrade ticket"),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_round_trip_and_reject_wrong_prefix_or_shape() {
        let id = CommandId::new();
        assert_eq!(CommandId::parse(id.as_str()).unwrap(), id);
        assert_eq!(id.to_string(), id.as_str());
        assert!(CommandId::parse("ses_00000000000000000000000000000000").is_err());
        assert!(CommandId::parse("cmd_0000000000000000000000000000000A").is_err());
        assert!(serde_json::from_str::<CommandId>(&format!("\"{}\"", id)).is_ok());
        assert!(
            serde_json::from_str::<CommandId>("\"00000000-0000-0000-0000-000000000000\"").is_err()
        );
        assert_eq!(
            CommandId::from_legacy_uuid("00000000-0000-0000-0000-000000000000")
                .unwrap()
                .as_str(),
            "cmd_00000000000000000000000000000000"
        );
    }
    #[test]
    fn prefixes_are_unique() {
        let mut p = ID_PREFIXES.iter().map(|x| x.0).collect::<Vec<_>>();
        p.sort();
        p.dedup();
        assert_eq!(p.len(), ID_PREFIXES.len());
    }

    #[test]
    fn every_resource_id_uses_its_declared_prefix() {
        assert!(FrontendId::new().as_str().starts_with("fe_"));
        assert!(DialogId::new().as_str().starts_with("dlg_"));
        assert!(WorkerSessionId::new().as_str().starts_with("ses_"));
        assert!(CommandId::new().as_str().starts_with("cmd_"));
        assert!(TaskId::new().as_str().starts_with("task_"));
        assert!(RunId::new().as_str().starts_with("run_"));
        assert!(GrantId::new().as_str().starts_with("grant_"));
        assert!(InstanceId::new().as_str().starts_with("ins_"));
        assert!(TabId::new().as_str().starts_with("tab_"));
        assert!(TerminalId::new().as_str().starts_with("term_"));
        assert!(LeaseId::new().as_str().starts_with("lease_"));
        assert!(KeyId::new().as_str().starts_with("key_"));
        assert!(BrowserLinkId::new().as_str().starts_with("blk_"));
        assert!(BrowserGrantId::new().as_str().starts_with("brw_"));
    }
}
