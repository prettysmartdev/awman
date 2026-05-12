//! Static audit prompt strings used by `init` and `ready`.
//!
//! These strings are sent to the agent as the seeded prompt for the
//! Dockerfile.dev audit run. Live in Layer 0 so both Layer-1 engines
//! (`InitEngine`, `ReadyEngine`) can consume them without crossing
//! into Layer 2.

/// Prompt used by `amux ready --build` and the init audit phase.
pub fn ready_audit_prompt() -> &'static str {
    "scan this project and determine every tool needed to build, run, \
and test it per the local development workflows defined in the aspec. Modify Dockerfile.dev \
to ensure that all of those tools, at the correct version, get installed when the Dockerfile \
is built. Pin to specific versions wherever possible. Ensure all relevant tools are in $PATH \
and can be executed by the container entrypoint command. Only modify Dockerfile.dev; do not \
modify any other files. Do not add any new files."
}

/// Prompt used by `amux init` for the post-build audit. Same as the ready
/// prompt today; isolated so it can diverge if the user-facing flow changes.
pub fn init_audit_prompt() -> &'static str {
    ready_audit_prompt()
}
