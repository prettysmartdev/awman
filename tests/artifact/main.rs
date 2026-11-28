//! `artifact` tier (WI 0144 design 8.6): the exact release executable under
//! a cleared environment, offline where the platform provides it. Budget:
//! 300 s. This binary must not `use awman` (hygiene rule H9): it links
//! neither the library nor the VM stack.
//!
//! Native runtime modules select only their reviewed host-specific session.

mod support;

mod apple_smoke;
mod docker_smoke;

mod deps_scan;
mod entrypoints;
mod measure;
mod sbx_smoke;
mod signed_boot;
mod smoke;
mod switches;
mod upgrade_policy;
