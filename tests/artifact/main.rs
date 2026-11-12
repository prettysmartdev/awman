//! `artifact` tier (WI 0144 design 8.6): the exact release executable under
//! a cleared environment, offline where the platform provides it. Budget:
//! 300 s. This binary must not `use awman` (hygiene rule H9): it links
//! neither the library nor the VM stack.
//!
//! The module list is final.

mod support;

mod deps_scan;
mod entrypoints;
mod measure;
mod signed_boot;
mod smoke;
mod switches;
mod upgrade_policy;
