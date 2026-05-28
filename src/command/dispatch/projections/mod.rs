//! Projections: per-frontend renderings of the canonical [`CommandCatalogue`].
//!
//! Frontends call only these methods; they MUST NEVER hard-code a command
//! name, flag name, or default value.

pub mod api_schema;
pub mod clap;
pub mod tui_hints;
