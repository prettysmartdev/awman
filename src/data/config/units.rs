//! Config-file spellings of durations and sizes.
//!
//! Every duration and size in a config file is a string with its unit in the
//! value (`"30s"`, `"4GiB"`), never a bare number with the unit in the key
//! name. These newtypes keep the user's spelling for round-tripping (a saved
//! file reads back byte for byte) and convert on demand.
//!
//! These are config spellings only. The HTTP API carries `_ms` / `_bytes`
//! integers (see `data::wire`).

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `"500ms" | "30s" | "15m" | "2h"`: one non-negative integer followed by one
/// unit, no spaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DurationStr {
    raw: String,
    duration: Duration,
}

impl DurationStr {
    /// Build from a whole number of seconds, spelled `"<n>s"`.
    pub fn from_secs(secs: u64) -> Self {
        Self {
            raw: format!("{secs}s"),
            duration: Duration::from_secs(secs),
        }
    }

    pub fn to_duration(&self) -> Duration {
        self.duration
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl FromStr for DurationStr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (digits, unit) = split_number(s).ok_or_else(|| duration_error(s))?;
        let n: u64 = digits.parse().map_err(|_| duration_error(s))?;
        let duration = match unit {
            "ms" => Duration::from_millis(n),
            "s" => Duration::from_secs(n),
            "m" => Duration::from_secs(n.checked_mul(60).ok_or_else(|| duration_error(s))?),
            "h" => Duration::from_secs(n.checked_mul(3600).ok_or_else(|| duration_error(s))?),
            _ => return Err(duration_error(s)),
        };
        Ok(Self {
            raw: s.to_string(),
            duration,
        })
    }
}

fn duration_error(s: &str) -> String {
    format!("{s:?} is not a duration; use a whole number and a unit, e.g. \"500ms\", \"30s\", \"15m\", \"2h\"")
}

/// `"512MiB" | "4GiB"`: one non-negative integer followed by a binary unit
/// (`KiB`, `MiB`, `GiB`, `TiB`), no spaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Size {
    raw: String,
    bytes: u64,
}

impl Size {
    /// Build from a whole number of MiB, spelled `"<n>MiB"`.
    pub fn from_mib(mib: u64) -> Self {
        Self {
            raw: format!("{mib}MiB"),
            bytes: mib * 1024 * 1024,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whole MiB, rounded down.
    pub fn mib(&self) -> u64 {
        self.bytes / (1024 * 1024)
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl FromStr for Size {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (digits, unit) = split_number(s).ok_or_else(|| size_error(s))?;
        let n: u64 = digits.parse().map_err(|_| size_error(s))?;
        let shift = match unit {
            "KiB" => 10,
            "MiB" => 20,
            "GiB" => 30,
            "TiB" => 40,
            _ => return Err(size_error(s)),
        };
        let bytes = n.checked_mul(1u64 << shift).ok_or_else(|| size_error(s))?;
        Ok(Self {
            raw: s.to_string(),
            bytes,
        })
    }
}

fn size_error(s: &str) -> String {
    format!("{s:?} is not a size; use a whole number and a binary unit, e.g. \"512MiB\", \"4GiB\"")
}

/// Split `"30s"` into `("30", "s")`. `None` without leading digits.
fn split_number(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit())?;
    if end == 0 {
        return None;
    }
    Some(s.split_at(end))
}

macro_rules! string_newtype_serde {
    ($ty:ty) => {
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.raw)
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.raw)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

string_newtype_serde!(DurationStr);
string_newtype_serde!(Size);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_each_unit() {
        assert_eq!(
            "500ms".parse::<DurationStr>().unwrap().to_duration(),
            Duration::from_millis(500)
        );
        assert_eq!(
            "30s".parse::<DurationStr>().unwrap().to_duration(),
            Duration::from_secs(30)
        );
        assert_eq!(
            "15m".parse::<DurationStr>().unwrap().to_duration(),
            Duration::from_secs(900)
        );
        assert_eq!(
            "2h".parse::<DurationStr>().unwrap().to_duration(),
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn durations_reject_bare_numbers_spaces_and_unknown_units() {
        for bad in ["30", "30 s", "s", "1.5s", "-1s", "3d", ""] {
            assert!(bad.parse::<DurationStr>().is_err(), "{bad}");
        }
    }

    #[test]
    fn sizes_parse_binary_units_only() {
        assert_eq!("4GiB".parse::<Size>().unwrap().mib(), 4096);
        assert_eq!("512MiB".parse::<Size>().unwrap().bytes(), 512 << 20);
        for bad in ["4GB", "4096", "4 GiB", "GiB"] {
            assert!(bad.parse::<Size>().is_err(), "{bad}");
        }
    }

    #[test]
    fn serde_round_trips_the_spelling() {
        let d: DurationStr = serde_json::from_str("\"60s\"").unwrap();
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"60s\"");
        let s: Size = serde_json::from_str("\"30GiB\"").unwrap();
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"30GiB\"");
        assert!(serde_json::from_str::<Size>("4096").is_err());
    }
}
