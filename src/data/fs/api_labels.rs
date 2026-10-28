//! Validated labels and query selectors used by API sessions and commands.

use std::collections::BTreeMap;

pub const MAX_LABELS: usize = 16;
/// Maximum UTF-8 byte length for each label key and value.
pub const MAX_LABEL_BYTES: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct Labels(BTreeMap<String, String>);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error("too many labels: {count} (max {max})", max = MAX_LABELS)]
    TooMany { count: usize },
    #[error("label key must not be empty")]
    KeyEmpty,
    #[error("label key '{key}' is {bytes} bytes (max {max})", max = MAX_LABEL_BYTES)]
    KeyTooLong { key: String, bytes: usize },
    #[error("label value for '{key}' is {bytes} bytes (max {max})", max = MAX_LABEL_BYTES)]
    ValueTooLong { key: String, bytes: usize },
    #[error("label key '{key}' must not contain '='")]
    KeyContainsEquals { key: String },
    #[error("label key or value for '{key}' contains a control character")]
    ControlCharacter { key: String },
}

impl Labels {
    pub fn new(map: BTreeMap<String, String>) -> Result<Self, LabelError> {
        if map.len() > MAX_LABELS {
            return Err(LabelError::TooMany { count: map.len() });
        }
        for (key, value) in &map {
            validate_key(key)?;
            validate_value(key, value)?;
        }
        Ok(Self(map))
    }

    pub fn empty() -> Self {
        Self(BTreeMap::new())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }
}

impl TryFrom<BTreeMap<String, String>> for Labels {
    type Error = LabelError;
    fn try_from(value: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> serde::Deserialize<'de> for Labels {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = <BTreeMap<String, String> as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(map).map_err(serde::de::Error::custom)
    }
}

fn validate_key(key: &str) -> Result<(), LabelError> {
    if key.is_empty() {
        return Err(LabelError::KeyEmpty);
    }
    if key.len() > MAX_LABEL_BYTES {
        return Err(LabelError::KeyTooLong {
            key: key.into(),
            bytes: key.len(),
        });
    }
    if key.contains('=') {
        return Err(LabelError::KeyContainsEquals { key: key.into() });
    }
    if key.chars().any(char::is_control) {
        return Err(LabelError::ControlCharacter { key: key.into() });
    }
    Ok(())
}

fn validate_value(key: &str, value: &str) -> Result<(), LabelError> {
    if value.len() > MAX_LABEL_BYTES {
        return Err(LabelError::ValueTooLong {
            key: key.into(),
            bytes: value.len(),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(LabelError::ControlCharacter { key: key.into() });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelSelector {
    Equals { key: String, value: String },
    Exists { key: String },
}

impl LabelSelector {
    pub fn parse(term: &str) -> Result<Self, LabelError> {
        match term.split_once('=') {
            Some((key, value)) => {
                validate_key(key)?;
                validate_value(key, value)?;
                Ok(Self::Equals {
                    key: key.into(),
                    value: value.into(),
                })
            }
            None => {
                validate_key(term)?;
                Ok(Self::Exists { key: term.into() })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_round_trips_labels() {
        let labels = Labels::new(BTreeMap::from([("env".into(), "prod".into())])).unwrap();
        assert_eq!(
            serde_json::from_str::<Labels>(&serde_json::to_string(&labels).unwrap()).unwrap(),
            labels
        );
        assert!(
            serde_json::from_str::<Labels>(&format!(r#"{{"x":"{}"}}"#, "a".repeat(257))).is_err()
        );
    }

    #[test]
    fn parses_exact_and_presence_selectors() {
        assert_eq!(
            LabelSelector::parse("env=prod").unwrap(),
            LabelSelector::Equals {
                key: "env".into(),
                value: "prod".into()
            }
        );
        assert_eq!(
            LabelSelector::parse("env").unwrap(),
            LabelSelector::Exists { key: "env".into() }
        );
        assert!(LabelSelector::parse("=prod").is_err());
    }

    #[test]
    fn enforces_pair_count_and_utf8_byte_boundaries() {
        let sixteen = (0..16)
            .map(|index| (format!("k{index}"), String::new()))
            .collect();
        assert_eq!(Labels::new(sixteen).unwrap().len(), 16);

        let seventeen = (0..17)
            .map(|index| (format!("k{index}"), String::new()))
            .collect();
        assert!(matches!(
            Labels::new(seventeen),
            Err(LabelError::TooMany { count: 17 })
        ));

        // 64 four-byte scalars are exactly 256 UTF-8 bytes. The next scalar
        // crosses the limit even though it is only one more character.
        let boundary = "🦀".repeat(64);
        assert_eq!(boundary.len(), MAX_LABEL_BYTES);
        assert!(Labels::new(BTreeMap::from([(boundary.clone(), boundary.clone())])).is_ok());

        let too_long = format!("{boundary}🦀");
        assert_eq!(too_long.len(), MAX_LABEL_BYTES + 4);
        assert!(matches!(
            Labels::new(BTreeMap::from([(too_long.clone(), String::new())])),
            Err(LabelError::KeyTooLong { bytes: 260, .. })
        ));
        assert!(matches!(
            Labels::new(BTreeMap::from([("key".into(), too_long)])),
            Err(LabelError::ValueTooLong { bytes: 260, .. })
        ));
    }
}
