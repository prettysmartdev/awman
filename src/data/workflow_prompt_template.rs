//! Workflow prompt-template substitution — Layer 0.
//!
//! Substitutes `{{work_item_number}}`, `{{work_item_content}}`,
//! `{{work_item_section:[Name]}}`, and `{{work_item}}` tokens against an
//! optional work-item context. Pure string transformation — no I/O.

/// Result of a substitution pass: the rendered prompt plus any warnings the
/// caller should forward to a `UserMessageSink`.
#[derive(Debug, Clone, Default)]
pub struct Substitution {
    pub rendered: String,
    pub warnings: Vec<String>,
}

/// Substitute every `{{...}}` placeholder in `template`. When `work_item` is
/// `None`, every `work_item_*` placeholder is replaced with an empty string
/// and a warning is queued so the caller can surface it via `UserMessageSink`.
pub fn substitute_prompt(template: &str, work_item: Option<&WorkItemContext>) -> Substitution {
    let mut out = template.to_string();
    let mut warnings = Vec::new();
    let uses_wi = template.contains("{{work_item");
    if uses_wi && work_item.is_none() {
        warnings.push(
            "workflow prompt references {{work_item_*}} but no --work-item was supplied; \
             placeholders rendered as empty strings"
                .to_string(),
        );
    }

    // Scan the original template once. Work item content is opaque data.
    out.clear();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("}}") else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let end = start + end + 2;
        let token = &rest[start + 2..end - 2];
        let replacement = match token {
            "work_item_number" => Some(
                work_item
                    .and_then(|wi| wi.number)
                    .map(|n| format!("{n:04}"))
                    .unwrap_or_default(),
            ),
            "work_item" => Some(
                work_item
                    .map(|wi| {
                        wi.number
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| wi.key.clone())
                    })
                    .unwrap_or_default(),
            ),
            "work_item_key" => Some(work_item.map(|wi| wi.key.clone()).unwrap_or_default()),
            "work_item_title" => Some(work_item.map(|wi| wi.title.clone()).unwrap_or_default()),
            "work_item_content" => Some(work_item.map(|wi| wi.content.clone()).unwrap_or_default()),
            token if token.starts_with("work_item_section:") => {
                let name = token["work_item_section:".len()..]
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .trim_end_matches(':')
                    .trim();
                Some(
                    work_item
                        .and_then(|wi| extract_section(&wi.content, name))
                        .unwrap_or_default(),
                )
            }
            _ => None,
        };
        out.push_str(replacement.as_deref().unwrap_or(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);

    Substitution {
        rendered: out,
        warnings,
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkItemContext {
    pub number: Option<u32>,
    pub key: String,
    pub title: String,
    pub source_id: String,
    pub content_hash: String,
    pub content: String,
}

impl WorkItemContext {
    pub fn from_legacy(number: u32, content: String) -> Self {
        Self {
            number: Some(number),
            key: format!("{number:04}"),
            title: String::new(),
            source_id: String::new(),
            content_hash: crate::data::fs::hash::sha256_hex(content.as_bytes()),
            content,
        }
    }

    /// Missing identity is accepted only for legacy numbered local states.
    pub fn validate_saved_identity(
        &self,
        state: &crate::data::workflow_state::WorkflowState,
    ) -> Result<(), crate::data::error::DataError> {
        let legacy_local = self.key.bytes().all(|b| b.is_ascii_digit());
        if state
            .work_item_source_id
            .as_deref()
            .is_some_and(|id| id != self.source_id)
            || (state.work_item_source_id.is_none() && !legacy_local)
            || state
                .work_item_key
                .as_deref()
                .is_some_and(|key| key != self.key)
        {
            return Err(crate::data::error::DataError::Other(
                "saved workflow belongs to a different work item; refusing to resume".into(),
            ));
        }
        Ok(())
    }
}

/// Extract the body of an H1 or H2 section whose heading matches `name`
/// case-insensitively (trailing colons stripped). Returns `None` when the
/// section is not found.
pub fn extract_section(content: &str, name: &str) -> Option<String> {
    let needle = name.trim().trim_end_matches(':').to_ascii_lowercase();
    let mut iter = content.lines().peekable();
    while let Some(line) = iter.next() {
        let trimmed = line.trim();
        let heading = trimmed
            .strip_prefix("## ")
            .or_else(|| trimmed.strip_prefix("# "));
        let Some(h) = heading else {
            continue;
        };
        let h_norm = h.trim().trim_end_matches(':').to_ascii_lowercase();
        if h_norm == needle {
            // Collect lines until the next H1/H2.
            let mut out = String::new();
            for next in iter.by_ref() {
                let nt = next.trim_start();
                if nt.starts_with("## ") || nt.starts_with("# ") {
                    break;
                }
                out.push_str(next);
                out.push('\n');
            }
            return Some(out.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;
    #[test]
    fn nonnumeric_identity_and_replacement_text_stay_data() {
        let context = WorkItemContext {
            number: None,
            key: "remote-1234567890abcdef12345678".into(),
            title: "{{work_item_content}}".into(),
            source_id: "https://tracker.example/items/PROJ-7".into(),
            content_hash: String::new(),
            content: "# Title\n\n## Summary\n{{work_item_title}} --work-item local:0".into(),
        };
        let result = substitute_prompt("{{work_item}}|{{work_item_number}}|{{work_item_key}}|{{work_item_title}}|{{work_item_content}}|{{work_item_section:[Summary]}}",Some(&context));
        assert_eq!(
            result.rendered,
            format!(
                "{}||{}|{}|{}|{{{{work_item_title}}}} --work-item local:0",
                context.key, context.key, context.title, context.content
            )
        );
        assert!(result.warnings.is_empty());
        let mut numbered = context;
        numbered.number = Some(42);
        assert_eq!(
            substitute_prompt("{{work_item_number}}/{{work_item}}", Some(&numbered)).rendered,
            "0042/42"
        );
        numbered.number = Some(12345);
        assert_eq!(
            substitute_prompt("{{work_item_number}}", Some(&numbered)).rendered,
            "12345"
        );
    }
}
