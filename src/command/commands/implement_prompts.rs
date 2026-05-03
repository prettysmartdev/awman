//! Prompt templates for `ImplementCommand` and `SpecsCommand`. Literal
//! strings are preserved from `oldsrc/commands/{implement,specs}.rs` so
//! user-visible prompts remain stable across the refactor.

/// Default single-step prompt used when `--workflow` is omitted.
/// `{{work_item_number}}` is substituted at command-build time.
pub const DEFAULT_IMPLEMENT_PROMPT: &str = "Implement work item {{work_item_number}}. Iterate \
    until the build succeeds. Implement tests as described in the work item and the project \
    aspec. Iterate until tests are comprehensive and pass. Write documentation as described \
    in the project aspec. Ensure final build and test success.";

/// Substitute the canonical placeholder.
pub fn render_default_prompt(work_item: &str) -> String {
    DEFAULT_IMPLEMENT_PROMPT.replace("{{work_item_number}}", work_item)
}

/// Interview prompt for `specs new --interview`. Ports `oldsrc/commands/
/// specs.rs::INTERVIEW_PROMPT_TEMPLATE` verbatim.
pub const INTERVIEW_PROMPT: &str = "Work item {number} template has been created for \
{kind}: {title}. Help complete the work item based on the following summary, making sure to \
include 1-3 concise user stories, detailed implementation plan, edge case considerations, \
test plan, and codebase integration tips. Only edit the work item markdown file, follow the \
template format. Do not edit any other files. Do not summarize your work at the end, let the \
user view the file themselves.\n\nSummary:\n{summary}";

/// Build the interview prompt for a new work item.
pub fn render_interview_prompt(number: u32, kind: &str, title: &str, summary: &str) -> String {
    INTERVIEW_PROMPT
        .replace("{number}", &format!("{number:04}"))
        .replace("{kind}", kind)
        .replace("{title}", title)
        .replace("{summary}", summary)
}

/// Amend prompt for `specs amend`. Ports `oldsrc/commands/specs.rs::
/// AMEND_PROMPT_TEMPLATE` verbatim.
pub const AMEND_PROMPT: &str = "Work item {number} is complete. Review the work that has \
been done in the codebase and compare it against the work item markdown file. If needed, amend \
the work item to ensure it matches the final implementation, ensuring completeness and \
correctness. Only edit the work item markdown file. Be concise and prefer leaving existing text \
as-is unless it is factually incorrect. Add new details if needed. Summarize the implementation \
and any corrections or changes that were needed to achieve the desired result in a new \
`Agent implementation notes` section at the bottom of the file.";

/// Build the amend prompt for a completed work item.
pub fn render_amend_prompt(number: u32) -> String {
    AMEND_PROMPT.replace("{number}", &format!("{number:04}"))
}
