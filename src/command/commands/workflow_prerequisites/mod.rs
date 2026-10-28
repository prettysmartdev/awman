//! Workflow prerequisites (WI 0149), Layer 2: the flow that turns the
//! engine's prerequisite report, the resolved policy and a frontend into the
//! run's grants; the frontend trait it asks through; the `--prerequisite-*`
//! flags; the API/squad widening check; and `config prerequisite-grants`.
//!
//! The probes themselves are `engine::workflow::prerequisites`; the policy
//! merge is `data::config::workflows`; turning grants into mounts and env is
//! `OverlayEngine::prerequisite_grant_overlays`. Nothing here prompts
//! directly: every question goes through [`WorkflowPrerequisiteFrontend`].

mod flags;
mod flow;
mod frontend;
mod grants_subcommand;
mod task_policy;
mod widening;

pub use flags::{
    flags_applied_policy, parse_identity, parse_prerequisite_flags, parse_ssh, parse_token,
    policy_lines, policy_summary, prerequisite_flag_args, validate_identity, FLAG_CHECK,
    FLAG_IDENTITY, FLAG_SSH, FLAG_TOKEN, FLAG_UNRESOLVED,
};
pub use flow::{
    custom_agent_binaries, headless_for, policy_for, repo_overlay_notes, PreparedPrerequisites,
    PrerequisiteOutcome, WorkflowPrerequisiteFlow, WorkflowPrerequisiteFlowOptions,
    ACQUIRE_TRUST_NAME,
};
pub use frontend::{
    GapChoice, IdentityChoice, IdentityPersist, SshChoice, TokenConsent, TokenSourceChoice,
    ToolChoice, WorkflowPrerequisiteFrontend,
};
pub use grants_subcommand::{
    parse_selector, run_prerequisite_grants, PrerequisiteGrantRow, PrerequisiteGrantsAction,
    PrerequisiteGrantsOutcome,
};
pub use task_policy::{collect_task_prerequisites, edit_task_policy, task_policy_from_outcome};
pub use widening::{check_flag_widening, check_task_widening};

#[cfg(test)]
mod tests;
