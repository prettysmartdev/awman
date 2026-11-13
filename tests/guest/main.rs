//! `guest` tier (WI 0144 design 8.5): a real VM on native hardware. Built
//! only with the `guest-drivers` feature (`builtin` feature set). Budget:
//! 300 s; VM concurrency is the nextest test group `vm`.
//!
//! The module list is final; `soak` holds explicitly tier-`soak` cases.

mod support;

mod boot;
mod builder;
mod exec;
mod import;
mod network;
mod soak;
mod strategy;
mod upgrade;
