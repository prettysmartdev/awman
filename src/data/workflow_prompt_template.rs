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

    // {{work_item_number}} → zero-padded four-digit
    out = replace_token(&out, "{{work_item_number}}", |_| match work_item {
        Some(wi) => format!("{:04}", wi.number),
        None => String::new(),
    });
    // {{work_item}} → bare numeric
    out = replace_token(&out, "{{work_item}}", |_| match work_item {
        Some(wi) => wi.number.to_string(),
        None => String::new(),
    });
    // {{work_item_content}} → full file body
    out = replace_token(&out, "{{work_item_content}}", |_| match work_item {
        Some(wi) => wi.content.clone(),
        None => String::new(),
    });
    // {{work_item_section:[Name]}} → body of the named section
    while let Some(start) = out.find("{{work_item_section:") {
        let end = match out[start..].find("}}") {
            Some(e) => start + e + 2,
            None => break,
        };
        let body_start = start + "{{work_item_section:".len();
        let body_end = end - 2;
        let raw = out[body_start..body_end].trim();
        let name = raw
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches(':')
            .trim();
        let replacement = match work_item {
            Some(wi) => extract_section(&wi.content, name).unwrap_or_default(),
            None => String::new(),
        };
        out.replace_range(start..end, &replacement);
    }

    Substitution {
        rendered: out,
        warnings,
    }
}

#[derive(Debug, Clone)]
pub struct WorkItemContext {
    pub number: u32,
    pub content: String,
}

fn replace_token<F: Fn(&str) -> String>(input: &str, token: &str, f: F) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = rest.find(token) {
        out.push_str(&rest[..idx]);
        out.push_str(&f(token));
        rest = &rest[idx + token.len()..];
    }
    out.push_str(rest);
    out
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
mod tests {
    use super::*;

    fn wi(content: &str) -> WorkItemContext {
        WorkItemContext {
            number: 42,
            content: content.into(),
        }
    }

    #[test]
    fn substitutes_zero_padded_number() {
        let sub = substitute_prompt("WI {{work_item_number}}", Some(&wi("body")));
        assert_eq!(sub.rendered, "WI 0042");
    }

    #[test]
    fn substitutes_bare_number() {
        let sub = substitute_prompt("WI {{work_item}}", Some(&wi("body")));
        assert_eq!(sub.rendered, "WI 42");
    }

    #[test]
    fn substitutes_content() {
        let sub = substitute_prompt("== {{work_item_content}} ==", Some(&wi("body")));
        assert_eq!(sub.rendered, "== body ==");
    }

    #[test]
    fn extracts_section() {
        let body = "# Title\n\n## Goal\nDo the thing\n\n## Notes\nN/A\n";
        let sub = substitute_prompt("Goal: {{work_item_section:[Goal]}}", Some(&wi(body)));
        assert_eq!(sub.rendered, "Goal: Do the thing");
    }

    #[test]
    fn warning_when_no_work_item() {
        let sub = substitute_prompt("WI {{work_item_number}}", None);
        assert_eq!(sub.rendered, "WI ");
        assert_eq!(sub.warnings.len(), 1);
    }
}
