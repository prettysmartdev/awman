//! Layer 0 network *addresses* — the fixed external endpoints awman talks to.
//!
//! No request is issued from here. WI 0114 F-29 moved the aspec download and
//! extraction up to `engine::aspec`; what remains is the constant naming the
//! tarball, which belongs beside awman's other external contracts.

pub mod aspec_tarball;

pub use aspec_tarball::ASPEC_TARBALL_URL;
