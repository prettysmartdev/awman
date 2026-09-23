//! `src/command/commands/` — one struct per awman command.
//!
//! Each module contains the `*Command` struct (owning every flag value and
//! engine reference it needs), its `*CommandFrontend` trait (defining the
//! exact user-input methods that command requires), and the
//! `Command` impl whose `run_with_frontend(frontend) -> *Outcome` body holds
//! all of the command's business logic.

pub mod agent_auth;
pub mod agent_setup;
pub mod api_server;
pub mod chat;
pub mod clean;
pub mod command_trait;
pub mod config;
pub mod exec_prompt;
pub mod exec_workflow;
pub mod squad;
pub mod workflow_preflight;
// `HttpCore` is Layer 1 (WI 0114 F-28) — the tree's one `reqwest::Client`
// builder, shared with the engine's own HTTP users. Re-exported here so
// daemon-facing Layer 2 clients and integration consumers keep the path they
// have always used.
pub use crate::engine::remote::HttpCore;
pub(crate) mod dynamic_repair;
// The WI-0092 leader/repair budget is the one decision core `exec workflow
// --dynamic` and the squad evaluator share; both callers — and the tests that
// prove they behave identically — reach it through this re-export.
pub use dynamic_repair::{RepairDecision, WorkflowRepairLoop};
pub mod init;
pub mod launch_policy;
pub mod mount_scope;
pub mod new;
pub mod prompt_templates;
pub mod ready;
pub mod remote;
pub mod remote_client;
pub mod skill_library;
pub mod specs;
pub mod status;
pub mod status_tips;
pub mod worktree_lifecycle;

pub use command_trait::Command;

pub use launch_policy::{LaunchModeDecision, LaunchPolicy};

// The overlay grammar is Layer 0 (WI 0114 F-27): `data::config::overlays`
// owns the parser, the parsed types and the six-source merge
// (`EffectiveConfig::collected_overlays`). These re-exports keep the names
// reachable at their long-standing `command::commands::…` paths.
pub use crate::data::config::overlays::{
    parse_overlay_list, parse_overlay_spec, CollectedOverlays, ContextOverlaySpec, ContextScope,
    SkillSpec, TypedOverlay,
};
