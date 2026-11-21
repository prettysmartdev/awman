//! Component tests: Layer 0 data and configuration.
//!
//! Owner: workflow step `slice-layer0-data`.

mod agent_catalogue;
mod config;
mod config_properties;
mod persistence;

mod context_store;
mod work_item_drafts;
mod work_item_files;

pub(crate) mod automation_store;
pub(crate) mod credential_updates;

mod agent_profiles;
mod private_files;
mod store_migration;
