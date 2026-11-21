//! Projections: per-frontend renderings of the canonical [`CommandCatalogue`].
//!
//! Frontends call only these methods; they MUST NEVER hard-code a command
//! name, flag name, or default value.

pub mod clap;
pub mod markdown;
#[cfg(test)]
mod optional_enum_tests;
pub mod raw_args;
pub mod tui_hints;
