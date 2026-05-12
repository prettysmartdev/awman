//! Layer 0 network helpers — pure download-and-extract utilities.
//!
//! These helpers are I/O concerns (network + filesystem) without any business
//! semantics; they are called by Layer 1 engines that compose them with
//! decision logic.

pub mod aspec_tarball;

pub use aspec_tarball::{
    download_aspec_tarball, extract_aspec_tarball, NetworkError, ASPEC_TARBALL_URL,
};
