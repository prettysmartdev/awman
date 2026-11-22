//! Rendering for the exec-workflow merge-fix prompt.

use crate::data::workflow_prompt_template::{substitute_prompt, Substitution, WorkItemContext};

/// Built-in merge-fix prompt (`src/assets/worktree/merge-fix-prompt.md`).
pub(super) const MERGE_FIX_PROMPT_MD: &str =
    include_str!("../../../assets/worktree/merge-fix-prompt.md");

/// The failed merge, as the prompt's placeholders see it.
#[derive(Debug, Clone, Copy)]
pub(super) struct MergeFixPromptVars<'a> {
    pub merge_command: &'a str,
    pub merge_exit_code: i32,
    pub merge_output: &'a str,
    pub branch: &'a str,
    pub target_branch: &'a str,
    pub unmerged_paths: &'a [String],
}

/// Render a merge-fix prompt template.
///
/// Substitutes the work-item placeholders first (`substitute_prompt`), then
/// `{{merge_command}}`, `{{merge_exit_code}}`, `{{merge_output}}`,
/// `{{branch}}`, `{{target_branch}}` and `{{unmerged_paths}}` (one path per
/// line, `(none)` when git listed none). The order matters: the merge output
/// is untrusted text and must not be able to smuggle in a work-item
/// placeholder.
pub(super) fn render_merge_fix_prompt(
    template: &str,
    vars: &MergeFixPromptVars<'_>,
    work_item: Option<&WorkItemContext>,
) -> Substitution {
    let Substitution { rendered, warnings } = substitute_prompt(template, work_item);
    let unmerged = if vars.unmerged_paths.is_empty() {
        "(none)".to_string()
    } else {
        vars.unmerged_paths.join("\n")
    };
    let exit_code = vars.merge_exit_code.to_string();
    let values = [
        ("{{merge_command}}", vars.merge_command),
        ("{{merge_exit_code}}", exit_code.as_str()),
        ("{{branch}}", vars.branch),
        ("{{target_branch}}", vars.target_branch),
        ("{{unmerged_paths}}", unmerged.as_str()),
        ("{{merge_output}}", vars.merge_output),
    ];
    // Scan only the template, never inserted values: branch names and filenames
    // can contain placeholders too.
    let mut out = String::new();
    let mut rest = rendered.as_str();
    while let Some((index, token, value)) = values
        .iter()
        .filter_map(|(token, value)| rest.find(token).map(|index| (index, *token, *value)))
        .min_by_key(|(index, _, _)| *index)
    {
        out.push_str(&rest[..index]);
        if template == MERGE_FIX_PROMPT_MD && token != "{{merge_exit_code}}" {
            // A fence longer than any run in the data cannot be closed by it.
            let width = value
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(2)
                + 1;
            let fence = "`".repeat(width);
            out.push_str(&format!("{fence}text\n{value}\n{fence}"));
        } else {
            out.push_str(value);
        }
        rest = &rest[index + token.len()..];
    }
    out.push_str(rest);
    Substitution {
        rendered: out,
        warnings,
    }
}
