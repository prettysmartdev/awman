//! `awman daemon`: the five verbs (`start`, `stop`, `status`, `logs`, `api`).
//! Action names exist only in this client (`actions.rs`).
pub mod actions;
pub mod api;
pub mod client;
pub mod lifecycle;
