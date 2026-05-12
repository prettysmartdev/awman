//! Prompt templates for `SpecsCommand` and `NewCommand`. Literal
//! strings are preserved from `oldsrc/commands/specs.rs` so
//! user-visible prompts remain stable across the refactor.

/// Interview prompt for `new spec --interview`. Ports `oldsrc/commands/
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

/// Interview prompt for `new workflow --interview`. Ported from
/// `oldsrc/commands/new_workflow.rs::WORKFLOW_INTERVIEW_PROMPT_TEMPLATE`.
pub const WORKFLOW_INTERVIEW_PROMPT: &str = "Workflow file {filename} has been created at \
{path}. Help complete the workflow based on the following summary. The workflow should include all \
necessary steps with clear step names, explicit depends_on relationships, appropriate agent and \
model choices where relevant, and detailed, actionable prompts for each step. Only edit the \
workflow file. Do not create or edit any other files. Follow the file format already present in \
the skeleton. Do not summarize your work at the end — let the user review the file themselves.\n\n\
Summary:\n{summary}";

/// Build the interview prompt for a new workflow file.
pub fn render_workflow_interview_prompt(filename: &str, path: &str, summary: &str) -> String {
    WORKFLOW_INTERVIEW_PROMPT
        .replace("{filename}", filename)
        .replace("{path}", path)
        .replace("{summary}", summary)
}

/// Interview prompt for `new skill --interview`. Ported from
/// `oldsrc/commands/new_skill.rs::SKILL_INTERVIEW_PROMPT_TEMPLATE`.
pub const SKILL_INTERVIEW_PROMPT: &str = "A skill file has been created at {path}. \
Help complete the skill based on the following summary. The skill should include clear \
instructions that a code agent can follow step-by-step, with any relevant commands, code \
examples, or decision trees needed. Write the skill in the second person imperative \
(\"Run ...\", \"Check ...\", \"If ... then ...\"). Only edit the skill file at {path}. \
Do not create or edit any other files. Follow the YAML frontmatter already present in the \
skeleton. Do not summarize your work at the end — let the user review the file themselves.\n\n\
Summary:\n{summary}";

/// Build the interview prompt for a new skill file.
pub fn render_skill_interview_prompt(path: &str, summary: &str) -> String {
    SKILL_INTERVIEW_PROMPT
        .replace("{path}", path)
        .replace("{summary}", summary)
}
