//! Layer 3 — frontends.
//!
//! Three independent implementations consume `Dispatch` (Layer 2),
//! `SessionManager` (Layer 0), and the per-command frontend traits
//! (Layers 1 + 2):
//!
//! - [`cli`]    — argv-driven, stdout/stderr/stdin rendering.
//! - [`tui`]    — Ratatui-based interactive terminal UI.
//! - [`api`] — HTTP server for programmatic / remote access.
//!
//! Frontends contain NO business logic; every behavioral decision lives in
//! Layer 2.

pub mod api;
pub mod cli;
pub mod daemon;

pub mod osc7501;
pub mod program_status_text;
pub mod render_helpers;
pub mod squad;
pub mod terminal_output_filter;
pub mod tui;

// The headless rule used to live here, as
// `effective_non_interactive(explicitly_requested)`. It is Layer 2's:
// `ResolvedFlags::is_headless` (WI 0114 F-50). A frontend reports
// whether there is anywhere to read an answer from —
// `CommandFrontend::input_available` — and nothing else.
