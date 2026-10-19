//! Safe wiring between native host integrations and the awman engines.
#![forbid(unsafe_code)]

#[cfg(any(target_os = "macos", test))]
mod apple_transport;
#[cfg(target_os = "macos")]
pub use apple_transport::NativeXpc;
