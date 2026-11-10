//! Kubernetes-style node affinity, evaluated again by the worker after claim.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Affinity {
    #[serde(default)]
    pub required: Vec<Match>,
    #[serde(default)]
    pub preferred: Vec<Preferred>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferred {
    pub weight: u8,
    #[serde(rename = "match", default)]
    pub matches: Vec<Match>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    pub key: String,
    pub op: MatchOp,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchOp {
    In,
    NotIn,
    Exists,
    DoesNotExist,
}

impl Match {
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        let value = labels.get(&self.key);
        match self.op {
            MatchOp::In => value.is_some_and(|v| self.values.contains(v)),
            MatchOp::NotIn => value.is_none_or(|v| !self.values.contains(v)),
            MatchOp::Exists => value.is_some(),
            MatchOp::DoesNotExist => value.is_none(),
        }
    }

    pub fn valid(&self) -> bool {
        let values_valid = match self.op {
            MatchOp::In | MatchOp::NotIn => !self.values.is_empty(),
            MatchOp::Exists | MatchOp::DoesNotExist => self.values.is_empty(),
        };
        values_valid && crate::labels::validate_key(&self.key).is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid affinity: invalid match or preferred weight outside 1..=100")]
pub struct AffinityError;
impl Affinity {
    pub fn validate(&self) -> Result<(), AffinityError> {
        if self.required.iter().all(Match::valid)
            && self
                .preferred
                .iter()
                .all(|p| (1..=100).contains(&p.weight) && p.matches.iter().all(Match::valid))
        {
            Ok(())
        } else {
            Err(AffinityError)
        }
    }
    pub fn eligible(&self, labels: &BTreeMap<String, String>) -> bool {
        self.required.iter().all(|m| m.valid() && m.matches(labels))
    }

    pub fn preference_score(&self, labels: &BTreeMap<String, String>) -> u32 {
        self.preferred
            .iter()
            .filter(|p| {
                (1..=100).contains(&p.weight)
                    && p.matches.iter().all(|m| m.valid() && m.matches(labels))
            })
            .map(|p| u32::from(p.weight))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_operators_and_missing_values_follow_node_affinity() {
        let labels = BTreeMap::from([("owner".into(), "github:alice".into())]);
        let case = |key: &str, op, values: &[&str]| Match {
            key: key.into(),
            op,
            values: values.iter().map(|s| (*s).into()).collect(),
        };
        assert!(case("owner", MatchOp::In, &["github:alice"]).matches(&labels));
        assert!(!case("owner", MatchOp::NotIn, &["github:alice"]).matches(&labels));
        assert!(case("owner", MatchOp::NotIn, &["github:bob"]).matches(&labels));
        assert!(case("missing", MatchOp::NotIn, &["github:bob"]).matches(&labels));
        assert!(case("owner", MatchOp::Exists, &[]).matches(&labels));
        assert!(case("missing", MatchOp::DoesNotExist, &[]).matches(&labels));
        assert!(!case("owner", MatchOp::Exists, &["unexpected"]).valid());
        assert!(!case("owner", MatchOp::In, &[]).valid());
    }

    #[test]
    fn required_and_preferred_are_separate() {
        let labels = BTreeMap::from([("pool".into(), "laptops".into())]);
        let affinity = Affinity {
            required: vec![Match {
                key: "pool".into(),
                op: MatchOp::Exists,
                values: vec![],
            }],
            preferred: vec![Preferred {
                weight: 25,
                matches: vec![Match {
                    key: "pool".into(),
                    op: MatchOp::In,
                    values: vec!["laptops".into()],
                }],
            }],
        };
        assert!(affinity.eligible(&labels));
        assert_eq!(affinity.preference_score(&labels), 25);
        assert!(!affinity.eligible(&BTreeMap::new()));
    }
}
