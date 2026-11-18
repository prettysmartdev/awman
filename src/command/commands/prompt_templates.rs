//! Shared authoring prompts. User-supplied text is substituted exactly once.
pub mod edit;

pub fn render_new_spec_prompt(
    path: &str,
    kind: &str,
    title: &str,
    summary: &str,
    headless: bool,
    remote: bool,
) -> String {
    use edit::*;
    let prompt = render_edit_prompt(&EditPromptRequest {
        subject: EditSubject::Spec { title },
        name: title,
        primary_path: path,
        companion_paths: &[],
        bundle_dir: None,
        location: if remote {
            EditLocation::RemoteDraft
        } else {
            EditLocation::InPlace
        },
        global: false,
        creating: true,
        amend: false,
        headless,
        brief: Some(summary),
    });
    format!("Work item type: {kind}.\n\n{prompt}")
}

/// Substitute every placeholder against the original template in one pass,
/// so a title or summary containing `{summary}` (work item text is untrusted)
/// is never expanded a second time.
pub(crate) fn render(template: &str, values: &[(&str, &str)]) -> String {
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
