//! Safe wiring between native host integrations and the awman engines.
#![forbid(unsafe_code)]

#[cfg(target_os = "macos")]
mod apple_transport;
#[cfg(target_os = "macos")]
pub use apple_transport::NativeXpc;
