//! Mock offer gating for optional lease exec fields.
use crate::types::WorkItem;

pub fn offer_allowed(registered: &[String], item: &WorkItem) -> bool {
    crate::lease_exec::supported(registered, item)
}
