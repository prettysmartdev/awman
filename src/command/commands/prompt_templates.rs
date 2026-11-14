//! Prompt templates for `SpecsCommand` and `NewCommand`. Keep these literal
//! strings stable because they are shown to users and passed to agents.

/// Interview prompt for `new spec --interview`.
///
/// `{path}` is the container path of the markdown file the agent edits: the
/// checkout file for local work items, or a draft that awman uploads to the
/// tracker for remote ones. The user may edit the same file, so the agent is
/// told to re-read it before every change.
pub const INTERVIEW_PROMPT: &str = "A {kind} work item titled \"{title}\" has been started \
from the work item template in the markdown file at {path}. Help complete the work item based \
on the following summary, making sure to include 1-3 concise user stories, detailed \
implementation plan, edge case considerations, test plan, and codebase integration tips. Only \
edit the work item markdown file at {path}, follow the template format. Re-read the file \
immediately before each change, because the user may be editing it at the same time. Do not \
edit any other files. Do not summarize your work at the end, let the user view the file \
themselves.\n\nSummary:\n{summary}";

/// Build the interview prompt for a new work item stored at `path`.
pub fn render_interview_prompt(path: &str, kind: &str, title: &str, summary: &str) -> String {
    render(
        INTERVIEW_PROMPT,
        &[
            ("{path}", path),
            ("{kind}", kind),
            ("{title}", title),
            ("{summary}", summary),
        ],
    )
}

/// Amend prompt for `specs amend`. `{path}` is the container path of the
/// markdown file to edit (see [`INTERVIEW_PROMPT`]).
pub const AMEND_PROMPT: &str = "The work item \"{title}\" is complete. Its markdown is in the \
file at {path}. Review the work that has been done in the codebase and compare it against that \
file. If needed, amend the work item to ensure it matches the final implementation, ensuring \
completeness and correctness. Only edit the work item markdown file at {path}. Re-read the file \
immediately before each change, because the user may be editing it at the same time. Be concise \
and prefer leaving existing text as-is unless it is factually incorrect. Add new details if \
needed. Summarize the implementation and any corrections or changes that were needed to achieve \
the desired result in a new `Agent implementation notes` section at the bottom of the file.";

/// Build the amend prompt for the completed work item stored at `path`.
pub fn render_amend_prompt(path: &str, title: &str) -> String {
    render(AMEND_PROMPT, &[("{path}", path), ("{title}", title)])
}

/// Substitute every placeholder against the original template in one pass,
/// so a title or summary containing `{summary}` (work item text is untrusted)
/// is never expanded a second time.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let mut rest = template;
    'outer: while !rest.is_empty() {
        for (key, value) in values {
            if let Some(after) = rest.strip_prefix(key) {
                out.push_str(value);
                rest = after;
                continue 'outer;
            }
        }
        let mut chars = rest.chars();
        out.push(chars.next().expect("non-empty"));
        rest = chars.as_str();
    }
    out
}

/// Interview prompt for `new workflow --interview`.
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

/// Interview prompt for `new skill --interview`.
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
