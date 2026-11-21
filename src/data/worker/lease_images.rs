//! Lease-built images and the eviction rule for them — Layer 0 (WI 0163).
//!
//! The worker records every image it builds for a lease, with when a lease
//! last used it, in its private state. After a lease ends and at worker
//! start it evicts lease-built images that no active lease uses, oldest
//! first, until the rest fit the server's [`ImageBudget`]. This module owns
//! the record shape and the pure selection; removing images and persisting
//! records belong to the layers above.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::image_inputs::{InputsHash, RepoKey};

/// Prepared immutable image references for a single lease session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseImageBinding {
    pub project: String,
    pub agents: std::collections::BTreeMap<String, String>,
}

/// Default `workServers.<name>.imageBudget.maxImages`.
pub const DEFAULT_MAX_LEASE_IMAGES: u32 = 20;

/// How many lease-built images a worker keeps. `None` on an axis means no
/// limit on it. Resolved from `workServers.<name>.imageBudget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_images: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl Default for ImageBudget {
    fn default() -> Self {
        Self {
            max_images: Some(DEFAULT_MAX_LEASE_IMAGES),
            max_bytes: None,
        }
    }
}

impl ImageBudget {
    /// Whether `count` images totalling `bytes` fit.
    pub fn admits(&self, count: usize, bytes: u64) -> bool {
        self.max_images.is_none_or(|max| count <= max as usize)
            && self.max_bytes.is_none_or(|max| bytes <= max)
    }
}

/// Whether a record is a project or an agent image.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseImageKind {
    Project,
    Agent { agent: String },
}

/// One image the worker built (or found and adopted) for a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseImageRecord {
    /// The content-keyed tag
    /// ([`crate::data::image_tags::lease_project_image_tag`] or
    /// [`crate::data::image_tags::lease_agent_image_tag`]).
    pub tag: String,
    /// The image id the tag pointed at when recorded.
    pub image_id: String,
    pub repo_key: RepoKey,
    pub inputs_hash: InputsHash,
    #[serde(flatten)]
    pub kind: LeaseImageKind,
    /// Image size as the store reports it; 0 when unknown.
    #[serde(default)]
    pub bytes: u64,
    /// When a lease last started on (or built) this image.
    pub last_used: DateTime<Utc>,
}

/// What [`select_evictions`] decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EvictionPlan {
    /// Tags to remove, oldest `last_used` first.
    pub evict: Vec<String>,
    /// Images that active leases use alone exceed the budget. They are
    /// kept; the caller warns once.
    pub budget_below_active: bool,
}

/// Choose lease-built images to remove so the rest fit `budget`.
///
/// Only `records` are candidates, so an image the worker did not build for
/// a lease is never chosen. Records whose tag is in `in_use` (tags of
/// images active leases run on) are never chosen. The rest are taken oldest
/// `last_used` first (ties by tag) until the remaining images fit. When the
/// in-use images alone exceed the budget, every other record is evicted and
/// [`EvictionPlan::budget_below_active`] is set.
pub fn select_evictions(
    records: &[LeaseImageRecord],
    in_use: &BTreeSet<String>,
    budget: &ImageBudget,
) -> EvictionPlan {
    // One record per tag; a duplicate keeps the newest use.
    let mut by_tag: std::collections::BTreeMap<&str, &LeaseImageRecord> = Default::default();
    for record in records {
        by_tag
            .entry(record.tag.as_str())
            .and_modify(|r| {
                if record.last_used > r.last_used {
                    *r = record;
                }
            })
            .or_insert(record);
    }
    let mut count = by_tag.len();
    let mut bytes: u64 = by_tag.values().map(|r| r.bytes).sum();

    let mut candidates: Vec<&LeaseImageRecord> = by_tag
        .values()
        .copied()
        .filter(|r| !in_use.contains(&r.tag))
        .collect();
    candidates.sort_by(|a, b| a.last_used.cmp(&b.last_used).then(a.tag.cmp(&b.tag)));

    let mut plan = EvictionPlan::default();
    for record in candidates {
        if budget.admits(count, bytes) {
            break;
        }
        plan.evict.push(record.tag.clone());
        count -= 1;
        bytes = bytes.saturating_sub(record.bytes);
    }
    plan.budget_below_active = !budget.admits(count, bytes);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn record(tag: &str, minute: u32, bytes: u64) -> LeaseImageRecord {
        LeaseImageRecord {
            tag: tag.to_string(),
            image_id: format!("sha256:{tag}"),
            repo_key: RepoKey::from_label("github.com/o/r"),
            inputs_hash: InputsHash::from_label("ab"),
            kind: LeaseImageKind::Project,
            bytes,
            last_used: Utc.with_ymd_and_hms(2026, 1, 1, 0, minute, 0).unwrap(),
        }
    }

    fn images(budget: Option<u32>) -> ImageBudget {
        ImageBudget {
            max_images: budget,
            max_bytes: None,
        }
    }

    #[test]
    fn default_budget_is_twenty_images() {
        assert_eq!(
            ImageBudget::default(),
            ImageBudget {
                max_images: Some(20),
                max_bytes: None
            }
        );
    }

    #[test]
    fn eviction_takes_oldest_first_until_within_max_images() {
        let records = [record("c", 3, 1), record("a", 1, 1), record("b", 2, 1)];
        let plan = select_evictions(&records, &BTreeSet::new(), &images(Some(1)));
        assert_eq!(plan.evict, vec!["a", "b"]);
        assert!(!plan.budget_below_active);
        let none = select_evictions(&records, &BTreeSet::new(), &images(Some(3)));
        assert!(none.evict.is_empty());
    }

    #[test]
    fn eviction_respects_max_bytes() {
        let records = [record("a", 1, 50), record("b", 2, 30), record("c", 3, 30)];
        let budget = ImageBudget {
            max_images: None,
            max_bytes: Some(60),
        };
        let plan = select_evictions(&records, &BTreeSet::new(), &budget);
        assert_eq!(plan.evict, vec!["a"]);
        let both = ImageBudget {
            max_images: Some(1),
            max_bytes: Some(1_000),
        };
        assert_eq!(
            select_evictions(&records, &BTreeSet::new(), &both).evict,
            vec!["a", "b"]
        );
    }

    #[test]
    fn eviction_never_selects_images_in_use_and_flags_a_too_small_budget() {
        let records = [
            record("old", 1, 1),
            record("mid", 2, 1),
            record("new", 3, 1),
        ];
        let in_use: BTreeSet<String> = ["old".to_string(), "new".to_string()].into();
        let plan = select_evictions(&records, &in_use, &images(Some(2)));
        assert_eq!(plan.evict, vec!["mid"]);
        assert!(!plan.budget_below_active);

        let plan = select_evictions(&records, &in_use, &images(Some(1)));
        assert_eq!(plan.evict, vec!["mid"]);
        assert!(plan.budget_below_active);
    }

    #[test]
    fn duplicate_records_count_once_with_their_newest_use() {
        let records = [record("a", 1, 1), record("b", 2, 1), record("a", 9, 1)];
        let plan = select_evictions(&records, &BTreeSet::new(), &images(Some(1)));
        assert_eq!(plan.evict, vec!["b"]);
    }

    #[test]
    fn records_round_trip_through_json() {
        let mut r = record("t", 1, 7);
        r.kind = LeaseImageKind::Agent {
            agent: "claude".into(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["kind"], "agent");
        assert_eq!(json["agent"], "claude");
        assert_eq!(json["repo_key"], "github.com/o/r");
        let back: LeaseImageRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
    }
}
