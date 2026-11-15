//! Work item sources (WI 0147): resolving `--work-item` references, staging
//! execution input, and editing remote work items through a synced draft.

pub mod draft_sync;
pub mod edit;
pub mod input;
pub mod resolver;

pub use input::{prepare_work_item_input, PreparedWorkItemInput};
pub use resolver::{
    canonical_source_id, find_work_item_file, is_local_reference, is_valid_work_item_key,
    local_path_work_item_key, local_work_item_key, parse_work_item_number, remote_work_item_key,
    ResolvedWorkItem, WorkItemCreationTarget, WorkItemError, WorkItemOrigin, WorkItemResolver,
};
