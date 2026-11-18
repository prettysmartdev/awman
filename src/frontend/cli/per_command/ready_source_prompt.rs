//! Line-mode presentation of the builtin image-source choice (WI 0158).
//!
//! Written against an injected line reader and writer so tests drive it
//! without a terminal. It only displays the request and returns the raw
//! answer: Layer 2 validates, Layer 0 computes the destination and writes.
//!
//! Reading maps to the typed answer like this: end of input (or any other
//! read failure) is `Eof`; an interrupt (Esc / Ctrl-C under a relay) is
//! `Cancelled`. An empty or unparseable menu answer selects the safe
//! default ("Leave unconfigured", "no", the first scope).

use std::io::Write;

use crate::data::config::image_source_prompt::{
    ImageSourceAnswer, ImageSourceChoiceRequest, ImageSourcePreview, ImageSourcePrompt,
    ImageSourceSelection, MissingSourceReason,
};

/// A value from the request (a path, tag, note, label or preview line) made
/// safe to print: terminal escape sequences and line breaks cannot reach
/// stderr. The answer returned keeps what the person typed.
fn safe(text: &str) -> String {
    crate::frontend::render_helpers::terminal_safe_line(text)
}

/// One read from the person.
enum Line {
    Text(String),
    Eof,
    Cancelled,
}

type ReadLine<'a> = dyn FnMut(&mut String) -> std::io::Result<usize> + 'a;

fn read(reader: &mut ReadLine<'_>, out: &mut dyn Write, prompt: &str) -> Line {
    let _ = write!(out, "{prompt}");
    let _ = out.flush();
    let mut buf = String::new();
    match reader(&mut buf) {
        Ok(0) => Line::Eof,
        Ok(_) => Line::Text(buf.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Line::Cancelled,
        Err(_) => Line::Eof,
    }
}

/// Present `prompt` and return the typed answer.
pub(crate) fn ask(
    prompt: &ImageSourcePrompt,
    reader: &mut ReadLine<'_>,
    out: &mut dyn Write,
) -> ImageSourceAnswer {
    match prompt {
        ImageSourcePrompt::Choose(request) => choose(request, reader, out),
        ImageSourcePrompt::Confirm(preview) => confirm(preview, reader, out),
    }
}

/// Map a non-text line to its terminal answer.
macro_rules! text_or_return {
    ($line:expr) => {
        match $line {
            Line::Text(text) => text,
            Line::Eof => return ImageSourceAnswer::Eof,
            Line::Cancelled => return ImageSourceAnswer::Cancelled,
        }
    };
}

fn choose(
    request: &ImageSourceChoiceRequest,
    reader: &mut ReadLine<'_>,
    out: &mut dyn Write,
) -> ImageSourceAnswer {
    let subject = &request.subject;
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "awman: no image source is configured for {} (agent {}).",
        safe(&subject.tag),
        safe(&subject.agent)
    );
    if let MissingSourceReason::DefaultPinned { kind } = &request.reason {
        let _ = writeln!(
            out,
            "awman: the default source is pinned to one image ({kind}), so it cannot serve this tag."
        );
    }
    for other in &request.also_missing {
        let _ = writeln!(
            out,
            "awman: {} (agent {}) also has no source.",
            safe(&other.tag),
            safe(&other.agent)
        );
    }
    for note in &request.notes {
        let _ = writeln!(out, "awman: {}", safe(note));
    }
    let _ = writeln!(out, "Where should awman import this image from?");
    for (i, choice) in request.choices.iter().enumerate() {
        let _ = writeln!(
            out,
            "  [{}] {} — {}",
            i + 1,
            choice.kind,
            safe(&choice.description)
        );
    }
    let leave = request.choices.len() + 1;
    let _ = writeln!(out, "  [{leave}] Leave unconfigured");

    let answer = text_or_return!(read(reader, out, &format!("Choice [{leave}]: ")));
    let picked = match answer.parse::<usize>() {
        Ok(n) if (1..=request.choices.len()).contains(&n) => n - 1,
        _ => return ImageSourceAnswer::LeaveUnconfigured,
    };
    let choice = &request.choices[picked];

    let mut fields = std::collections::BTreeMap::new();
    for field in &choice.fields {
        let _ = writeln!(out, "{} — {}", safe(&field.label), safe(&field.help));
        let marker = if field.required {
            ""
        } else {
            " (optional, Enter to skip)"
        };
        let value = text_or_return!(read(
            reader,
            out,
            &format!("{}{marker}: ", safe(&field.label))
        ));
        if !value.is_empty() {
            fields.insert(field.id, value);
        }
    }

    let mut scope = request.scopes[0].scope;
    if request.scopes.len() > 1 {
        let _ = writeln!(out, "Where should the setting be saved?");
        for (i, option) in request.scopes.iter().enumerate() {
            let _ = writeln!(
                out,
                "  [{}] {} config — {}",
                i + 1,
                option.scope.as_str(),
                safe(&option.path.display().to_string())
            );
        }
        let answer = text_or_return!(read(reader, out, "Scope [1]: "));
        let index = match answer.parse::<usize>() {
            Ok(n) if (1..=request.scopes.len()).contains(&n) => n - 1,
            _ => 0,
        };
        scope = request.scopes[index].scope;
    } else {
        let only = &request.scopes[0];
        let _ = writeln!(
            out,
            "awman: it will be saved to the {} config ({}).",
            only.scope.as_str(),
            safe(&only.path.display().to_string())
        );
    }

    let mut enable_builder = false;
    if let Some(offer) = &choice.builder_offer {
        let _ = writeln!(out, "{}", safe(offer));
        let answer = text_or_return!(read(reader, out, "Enable the builder? [y/N]: "));
        enable_builder = matches!(answer.as_str(), "y" | "Y" | "yes" | "Yes");
    }

    ImageSourceAnswer::Selected(ImageSourceSelection {
        kind: choice.kind,
        fields,
        scope,
        enable_builder,
    })
}

fn confirm(
    preview: &ImageSourcePreview,
    reader: &mut ReadLine<'_>,
    out: &mut dyn Write,
) -> ImageSourceAnswer {
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "awman: this will set {} in the {} config:",
        safe(&preview.field),
        preview.scope.as_str()
    );
    let _ = writeln!(out, "  file: {}", safe(&preview.path.display().to_string()));
    for line in preview.fragment.lines() {
        let _ = writeln!(out, "  {}", safe(line));
    }
    let _ = writeln!(out, "After saving:");
    for line in preview.effective_lines() {
        let _ = writeln!(out, "  {}", safe(&line));
    }
    for note in &preview.notes {
        let _ = writeln!(out, "awman: {}", safe(note));
    }
    let answer = text_or_return!(read(reader, out, "Save this change? [y/N]: "));
    if matches!(answer.as_str(), "y" | "Y" | "yes" | "Yes") {
        ImageSourceAnswer::Confirmed
    } else {
        ImageSourceAnswer::LeaveUnconfigured
    }
}

#[cfg(test)]
mod tests;
