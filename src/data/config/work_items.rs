//! `workItems` section: where work items and their template live.

use serde::{Deserialize, Serialize};

/// Default work items directory, relative to the repo root.
pub const DEFAULT_WORK_ITEMS_DIR: &str = "aspec/work-items";

/// Template file name inside the work items directory.
pub const DEFAULT_WORK_ITEM_TEMPLATE: &str = "0000-template.md";

/// The `workItems` block. (`workItems.source` and `workItems.providers` are
/// reserved for a later item.)
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkItemsConfig {
    /// Work items directory (relative to the repo root, or absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Work item template (relative to the repo root, or absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}
