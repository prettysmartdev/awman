//! Dialog presentation of the builtin image-source choice (WI 0158) for the
//! TUI and the relayed CLI.
//!
//! The request's context (subject, reason, notes) goes to the status log, as
//! the CLI prints it to stderr; the questions are dialogs. Each stage is
//! written against an `ask` closure so tests script the answers. It only
//! displays the request and returns the raw answer: Layer 2 validates and
//! Layer 0 computes the destination and writes.
//!
//! Any dialog failure, Esc, dismissal, interrupt or unexpected response is
//! `Cancelled`. A "no" to the builder question is a normal answer.

use crate::data::config::image_source_prompt::{
    ImageSourceAnswer, ImageSourceChoiceRequest, ImageSourcePreview, ImageSourcePrompt,
    ImageSourceSelection, MissingSourceReason,
};
use crate::data::dialog::{DialogRequest, DialogResponse, ReviewRequest};

/// Sends one dialog and waits for its response; `None` when it failed.
pub(super) type Ask<'a> = dyn FnMut(DialogRequest) -> Option<DialogResponse> + 'a;

pub(super) const SAVE_KEY: char = 's';
pub(super) const DONT_SAVE_KEY: char = 'n';

pub(super) fn ask_prompt(prompt: &ImageSourcePrompt, ask: &mut Ask<'_>) -> ImageSourceAnswer {
    match prompt {
        ImageSourcePrompt::Choose(request) => choose(request, ask),
        ImageSourcePrompt::Confirm(preview) => confirm(preview, ask),
    }
}

/// One line of untrusted text (a path, tag, note or error) made safe to
/// show: every control character, including newlines and the ESC that starts
/// a terminal escape sequence, becomes U+FFFD. Only presentation copies are
/// cleaned; the request and the answer keep the original values.
pub(super) fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// The context shown in the status log before stage 1's dialogs: the
/// subject, why it has no source, other missing images, the request's notes
/// and, when there is a single scope, where the setting will be saved.
pub(super) fn context_lines(request: &ImageSourceChoiceRequest) -> Vec<String> {
    let subject = &request.subject;
    let mut lines = vec![format!(
        "no image source is configured for {} (agent {}).",
        clean(&subject.tag),
        clean(&subject.agent)
    )];
    if let MissingSourceReason::DefaultPinned { kind } = &request.reason {
        lines.push(format!(
            "the default source is pinned to one image ({kind}), so it cannot serve this tag."
        ));
    }
    for other in &request.also_missing {
        lines.push(format!(
            "{} (agent {}) also has no source.",
            clean(&other.tag),
            clean(&other.agent)
        ));
    }
    lines.extend(request.notes.iter().map(|note| clean(note)));
    if let [only] = request.scopes.as_slice() {
        lines.push(format!(
            "the setting will be saved to the {} config ({}).",
            only.scope.as_str(),
            clean(&only.path.display().to_string())
        ));
    }
    lines
}

/// The picked index of a `ListPicker` among `len` items, else `None`.
fn index_of(response: Option<DialogResponse>, len: usize) -> Option<usize> {
    match response {
        Some(DialogResponse::Index(i)) if i < len => Some(i),
        _ => None,
    }
}

fn choose(request: &ImageSourceChoiceRequest, ask: &mut Ask<'_>) -> ImageSourceAnswer {
    let subject = &request.subject;
    // "Leave unconfigured" is the first entry: the TUI picker opens on its
    // first item and the relayed CLI's Enter picks item 1, so an immediate
    // Enter selects the safe default, as in the direct CLI.
    let mut items = vec!["Leave unconfigured".to_string()];
    items.extend(
        request
            .choices
            .iter()
            .map(|c| format!("{} — {}", c.kind, clean(&c.description))),
    );
    let response = ask(DialogRequest::ListPicker {
        title: format!("Image source for {}", clean(&subject.tag)),
        items: items.clone(),
    });
    let Some(picked) = index_of(response, items.len()) else {
        return ImageSourceAnswer::Cancelled;
    };
    let Some(choice) = picked
        .checked_sub(1)
        .and_then(|index| request.choices.get(index))
    else {
        return ImageSourceAnswer::LeaveUnconfigured;
    };

    let mut fields = std::collections::BTreeMap::new();
    for field in &choice.fields {
        let optional = if field.required { "" } else { " (optional)" };
        let response = ask(DialogRequest::TextInput {
            title: format!("{}{optional}", clean(&field.label)),
            prompt: clean(&field.help),
            default_text: None,
        });
        match response {
            Some(DialogResponse::Text(text)) => {
                let text = text.trim();
                if !text.is_empty() {
                    fields.insert(field.id, text.to_string());
                }
            }
            _ => return ImageSourceAnswer::Cancelled,
        }
    }

    let mut scope = request.scopes[0].scope;
    if request.scopes.len() > 1 {
        let items: Vec<String> = request
            .scopes
            .iter()
            .map(|o| {
                format!(
                    "{} config — {}",
                    o.scope.as_str(),
                    clean(&o.path.display().to_string())
                )
            })
            .collect();
        let response = ask(DialogRequest::ListPicker {
            title: "Save the setting to".into(),
            items: items.clone(),
        });
        let Some(index) = index_of(response, items.len()) else {
            return ImageSourceAnswer::Cancelled;
        };
        scope = request.scopes[index].scope;
    }

    let mut enable_builder = false;
    if let Some(offer) = &choice.builder_offer {
        match ask(DialogRequest::YesNo {
            title: "Enable the builder?".into(),
            body: clean(offer),
        }) {
            Some(DialogResponse::Yes) | Some(DialogResponse::Char('y')) => enable_builder = true,
            Some(DialogResponse::No) | Some(DialogResponse::Char('n')) => {}
            _ => return ImageSourceAnswer::Cancelled,
        }
    }

    ImageSourceAnswer::Selected(ImageSourceSelection {
        kind: choice.kind,
        fields,
        scope,
        enable_builder,
    })
}

/// The scrolling body of the stage-2 dialog: destination, the exact JSON,
/// and what the images resolve to afterwards.
pub(super) fn confirm_body(preview: &ImageSourcePreview) -> String {
    let mut body = format!(
        "This sets {} in the {} config.\nFile: {}\n\n{}\n\nAfter saving:\n",
        clean(&preview.field),
        preview.scope.as_str(),
        clean(&preview.path.display().to_string()),
        preview
            .fragment
            .lines()
            .map(clean)
            .collect::<Vec<_>>()
            .join("\n")
    );
    for line in preview.effective_lines() {
        body.push_str("  ");
        body.push_str(&clean(&line));
        body.push('\n');
    }
    for note in &preview.notes {
        body.push('\n');
        body.push_str(&clean(note));
    }
    body
}

fn confirm(preview: &ImageSourcePreview, ask: &mut Ask<'_>) -> ImageSourceAnswer {
    let response = ask(DialogRequest::Review(ReviewRequest::new(
        "Save image source?".into(),
        confirm_body(preview),
        vec![
            (SAVE_KEY, "Save".into()),
            (DONT_SAVE_KEY, "Don't save".into()),
        ],
        vec![],
    )));
    match response {
        Some(DialogResponse::Char(SAVE_KEY)) => ImageSourceAnswer::Confirmed,
        Some(DialogResponse::Char(DONT_SAVE_KEY)) => ImageSourceAnswer::LeaveUnconfigured,
        _ => ImageSourceAnswer::Cancelled,
    }
}

#[cfg(test)]
mod tests;
