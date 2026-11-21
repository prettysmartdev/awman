//! Shared wire shapes and serde adapters for resource APIs.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct List<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub limit: usize,
    pub offset_or_key: Option<Cursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("limit must be between 1 and 500")]
    InvalidLimit,
    #[error("invalid cursor")]
    InvalidCursor,
}

impl ListQuery {
    pub const DEFAULT_LIMIT: usize = 50;
    pub const MAX_LIMIT: usize = 500;
    pub fn page(&self) -> Result<Page, WireError> {
        let limit = self.limit.unwrap_or(Self::DEFAULT_LIMIT);
        if limit == 0 || limit > Self::MAX_LIMIT {
            return Err(WireError::InvalidLimit);
        }
        let offset_or_key = self.cursor.as_deref().map(Cursor::parse).transpose()?;
        Ok(Page {
            limit,
            offset_or_key,
        })
    }
}
impl Cursor {
    fn make(kind: &str, value: &str) -> Self {
        Self(URL_SAFE_NO_PAD.encode(format!("{kind}:{value}")))
    }
    pub fn offset(n: usize) -> Self {
        Self::make("offset", &n.to_string())
    }
    pub fn key(key: &str) -> Self {
        Self::make("key", key)
    }
    pub fn parse(s: &str) -> Result<Self, WireError> {
        let decoded = URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| WireError::InvalidCursor)?;
        let decoded = std::str::from_utf8(&decoded).map_err(|_| WireError::InvalidCursor)?;
        let (kind, value) = decoded.split_once(':').ok_or(WireError::InvalidCursor)?;
        match kind {
            "offset" if value.parse::<usize>().is_ok() => {}
            "key" if !value.is_empty() => {}
            _ => return Err(WireError::InvalidCursor),
        }
        Ok(Self(s.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn offset_value(&self) -> Option<usize> {
        let raw = URL_SAFE_NO_PAD.decode(&self.0).ok()?;
        let text = std::str::from_utf8(&raw).ok()?;
        text.strip_prefix("offset:")?.parse().ok()
    }
    pub fn key_value(&self) -> Option<String> {
        let raw = URL_SAFE_NO_PAD.decode(&self.0).ok()?;
        let text = std::str::from_utf8(&raw).ok()?;
        Some(text.strip_prefix("key:")?.to_owned())
    }
}
impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl<T> List<T> {
    pub fn page_from(all: Vec<T>, page: &Page, next: impl Fn(&T) -> Cursor) -> Self {
        let total = all.len();
        let start = page
            .offset_or_key
            .as_ref()
            .and_then(Cursor::offset_value)
            .unwrap_or(0)
            .min(total);
        let end = start.saturating_add(page.limit).min(total);
        let items: Vec<T> = all.into_iter().skip(start).take(end - start).collect();
        let next_cursor = if end < total {
            items.last().map(next).map(|c| c.to_string())
        } else {
            None
        };
        Self { items, next_cursor }
    }
}

pub mod duration_ms {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(v.as_millis().try_into().unwrap_or(u64::MAX))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}
pub mod opt_duration_ms {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_some(&u64::try_from(v.as_millis()).unwrap_or(u64::MAX)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Duration>, D::Error> {
        Option::<u64>::deserialize(d).map(|v| v.map(Duration::from_millis))
    }
}
pub mod timestamp {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_timestamp(*v))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let raw = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&raw)
            .map(|v| v.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}
pub mod opt_timestamp {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        v: &Option<DateTime<Utc>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_some(&format_timestamp(*v)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|raw| {
                DateTime::parse_from_rfc3339(&raw)
                    .map(|v| v.with_timezone(&Utc))
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}
pub mod bytes {
    pub fn serialize<S: serde::Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(*v)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        u64::deserialize(d)
    }
    use serde::Deserialize;
}
pub fn now() -> DateTime<Utc> {
    DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).expect("current UTC time")
}
pub fn format_timestamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}
pub fn assert_round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug>(
    value: &T,
) {
    let json = serde_json::to_string(value).expect("serialize");
    let decoded: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&decoded, value);
}
