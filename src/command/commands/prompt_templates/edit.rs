//! Editing prompts for `awman edit spec|workflow|skill|agent`.
//!
//! These prompts describe an artifact that already exists; none of them claims
//! that a skeleton was just created. Every prompt is built in one pass: static
//! fragments are chosen from the request and joined into a template that holds
//! only placeholders, then each placeholder is substituted exactly once with
//! [`render`]. User text (titles, briefs, names, paths) is only ever a value,
//! so placeholder-looking text inside it is never expanded.

use super::render;

/// What is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditSubject<'a> {
    /// A work item; `title` is untrusted provider/file text.
    Spec {
        title: &'a str,
    },
    /// `format` is "TOML" or "YAML".
    Workflow {
        format: &'a str,
    },
    Skill,
    Agent,
}

/// Where the editable file lives from the agent's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditLocation {
    /// The real file, in the checkout.
    InPlace,
    /// A draft copy awman saves back to the original after the session.
    StagedCopy,
    /// A work-item draft awman uploads to the tracker while the agent runs.
    RemoteDraft,
}

/// Everything needed to render one editing prompt.
#[derive(Debug, Clone)]
pub struct EditPromptRequest<'a> {
    pub subject: EditSubject<'a>,
    /// Identity shown to the agent (workflow/skill/agent name; unused for specs).
    pub name: &'a str,
    /// Container path of the main file.
    pub primary_path: &'a str,
    /// Container paths of further exact files the agent may edit (agent Dockerfile).
    pub companion_paths: &'a [String],
    /// Container path of a directory whose files may be edited or added (skills).
    pub bundle_dir: Option<&'a str>,
    pub location: EditLocation,
    /// Global or library artifact: must not become specific to this repository.
    pub global: bool,
    pub interview: bool,
    pub amend: bool,
    pub headless: bool,
    /// `--prompt` text; untrusted.
    pub brief: Option<&'a str>,
}

/// Build the editing prompt for `request`.
pub fn render_edit_prompt(request: &EditPromptRequest<'_>) -> String {
    let brief = request.brief.map(str::trim).filter(|b| !b.is_empty());
    let mut parts: Vec<String> = Vec::new();

    parts.push(intro(request).to_string());
    if let Some(location) = location_text(request.location) {
        parts.push(location.to_string());
    }
    if !request.companion_paths.is_empty() {
        parts.push(
            "You may also edit these exact companion files, and no others:\n{files}".to_string(),
        );
    }
    if request.bundle_dir.is_some() {
        parts.push(
            "The skill is the directory {dir}. You may edit the files inside it, and add new \
supporting files inside it when the skill needs them. Do not touch anything outside that \
directory."
                .to_string(),
        );
    }
    if request.global {
        parts.push(
            "This is a global artifact shared across repositories. Keep it generic: do not make \
it specific to the repository you are running in unless the user asks for that."
                .to_string(),
        );
    }

    parts.push(task_text(request, brief.is_some()));
    if request.amend {
        parts.push(amend_text(request.subject).to_string());
        parts.push(amend_common_text(request).to_string());
    }
    parts.push(boundaries_text(request).to_string());
    if brief.is_some() {
        parts.push(brief_text(request).to_string());
    }

    let template = parts.join("\n\n");
    let files = request
        .companion_paths
        .iter()
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (title, format) = match request.subject {
        EditSubject::Spec { title } => (title, ""),
        EditSubject::Workflow { format } => ("", format),
        _ => ("", ""),
    };
    render(
        &template,
        &[
            ("{path}", request.primary_path),
            ("{files}", &files),
            ("{dir}", request.bundle_dir.unwrap_or("")),
            ("{name}", request.name),
            ("{title}", title),
            ("{format}", format),
            ("{brief}", brief.unwrap_or("")),
        ],
    )
}

fn intro(request: &EditPromptRequest<'_>) -> &'static str {
    match request.subject {
        EditSubject::Spec { .. } => {
            "You are editing the existing work item \"{title}\". Its markdown is in the file at \
{path}. This is the only file you may edit."
        }
        EditSubject::Workflow { .. } => {
            "You are editing the existing workflow \"{name}\". It is a {format} workflow \
definition in the file at {path}. This is the only file you may edit."
        }
        EditSubject::Skill => {
            "You are editing the existing skill \"{name}\". Its instructions are in the file at \
{path}."
        }
        EditSubject::Agent => {
            "You are editing the existing custom agent definition \"{name}\". Its TOML definition \
is in the file at {path}."
        }
    }
}

fn location_text(location: EditLocation) -> Option<&'static str> {
    match location {
        EditLocation::InPlace => None,
        EditLocation::StagedCopy => Some(
            "The file is a working copy of the original. awman saves your changes back to the \
original after the session ends, so edit only the working copy.",
        ),
        EditLocation::RemoteDraft => Some(
            "The file is a draft of a work item stored in an issue tracker. awman uploads the \
draft to the tracker while you work, so keep the file complete and valid at all times. Do not \
add a `# <title>` heading; the title is stored separately.",
        ),
    }
}

fn task_text(request: &EditPromptRequest<'_>, has_brief: bool) -> String {
    let mut out = String::new();
    if request.interview {
        out.push_str(
            "Before you edit anything, interview the user: ask questions to clarify their intent, \
the constraints that apply, and the acceptance criteria for the change. Ask a few focused \
questions at a time and wait for the answers. Only start editing once you understand what the \
user wants.",
        );
        if has_brief {
            out.push_str(" Use the change brief below as the starting point for your questions.");
        }
    } else if request.amend {
        out.push_str(
            "Your task is to review this item against the current state of the repository and \
amend it so that it is accurate.",
        );
    } else if has_brief {
        out.push_str("Apply the change described in the change brief below.");
    } else {
        out.push_str(
            "Start by asking the user what they would like to change, then make that change.",
        );
    }
    if request.interview && request.amend {
        out.push_str(
            "\n\nThis is also an amendment: review the item against the current state of the \
repository first. Discuss every uncertain finding with the user through the interview, then \
reconcile the item with the evidence and the answers you receive.",
        );
    }
    out
}

fn amend_text(subject: EditSubject<'_>) -> &'static str {
    match subject {
        EditSubject::Spec { .. } => {
            "Amendment scope for a work item: review the implementation, the tests, the current \
behaviour and any available completion evidence, and compare them with the work item. Correct \
descriptions and implementation notes that are wrong. Identify unfinished work without claiming \
that it is complete. Keep useful implementation notes in Markdown inside the work item."
        }
        EditSubject::Workflow { .. } => {
            "Amendment scope for a workflow: review the scripts, commands, paths, dependencies \
and agent definitions that its steps reference. Correct stale steps, references and \
configuration. Preserve the workflow's intended purpose and keep it in its existing {format} \
format."
        }
        EditSubject::Skill => {
            "Amendment scope for a skill: review the current repository conventions, APIs, \
commands and supporting files that the skill refers to. Correct its instructions, examples, \
references and relevant supporting content."
        }
        EditSubject::Agent => {
            "Amendment scope for an agent definition: review the definition schema, the \
Dockerfile or install recipe, and any available runtime or configuration evidence. Correct the \
definition and the companion files listed above. Never build, install or run the agent."
        }
    }
}

fn amend_common_text(request: &EditPromptRequest<'_>) -> &'static str {
    if request.headless {
        "Distinguish verified facts, intended future behaviour and unknowns. Do not assume that \
work is complete, that tools are installed or that production behaviour has been observed. \
Preserve unresolved requirements even when the implementation is missing; do not delete them \
for that reason. A change brief may narrow the review, but it never justifies inventing facts. \
This is a headless session and nobody can answer questions: when the evidence is insufficient, \
report the limitation and make no speculative change. Never add spec-only sections, such as \
`Agent implementation notes`, to workflows, agent TOML or skill frontmatter."
    } else {
        "Distinguish verified facts, intended future behaviour and unknowns. Do not assume that \
work is complete, that tools are installed or that production behaviour has been observed. \
Preserve unresolved requirements even when the implementation is missing; do not delete them \
for that reason. A change brief may narrow the review, but it never justifies inventing facts. \
When the evidence is insufficient, ask the user instead of guessing. Never add spec-only \
sections, such as `Agent implementation notes`, to workflows, agent TOML or skill frontmatter."
    }
}

fn boundaries_text(request: &EditPromptRequest<'_>) -> &'static str {
    let scope = if request.bundle_dir.is_some() || !request.companion_paths.is_empty() {
        "Only edit the files named above."
    } else {
        "Only edit the file at {path}."
    };
    // Static text; `scope` is chosen from the request, never from user input.
    match scope {
        "Only edit the files named above." => {
            "Only edit the files named above. Preserve all content that is unrelated to the \
change. The user may be editing the same files at the same time, so re-read a file immediately \
before each change you make to it. You may read related code to understand the change, but do \
not implement features that the item describes and do not change unrelated code or files. Do \
not summarize your work at the end; let the user review the result themselves."
        }
        _ => {
            "Only edit the file at {path}. Preserve all content that is unrelated to the change. \
The user may be editing the same file at the same time, so re-read it immediately before each \
change you make. You may read related code to understand the change, but do not implement \
features that the item describes and do not change unrelated code or files. Do not summarize \
your work at the end; let the user review the result themselves."
        }
    }
}

fn brief_text(request: &EditPromptRequest<'_>) -> &'static str {
    if request.amend {
        "Change brief (it may narrow the scope of the review, and it is not evidence):\n{brief}"
    } else {
        "Change brief:\n{brief}"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(subject: EditSubject<'a>) -> EditPromptRequest<'a> {
        EditPromptRequest {
            subject,
            name: "demo",
            primary_path: "/workspace/x/main.md",
            companion_paths: &[],
            bundle_dir: None,
            location: EditLocation::InPlace,
            global: false,
            interview: false,
            amend: false,
            headless: false,
            brief: None,
        }
    }

    fn subjects() -> Vec<EditSubject<'static>> {
        vec![
            EditSubject::Spec { title: "Title" },
            EditSubject::Workflow { format: "TOML" },
            EditSubject::Skill,
            EditSubject::Agent,
        ]
    }

    #[test]
    fn every_kind_and_mode_names_file_and_safety_rules() {
        for subject in subjects() {
            for (interview, amend) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut r = req(subject);
                r.interview = interview;
                r.amend = amend;
                r.brief = Some("make it better");
                let p = render_edit_prompt(&r);
                assert!(p.contains("/workspace/x/main.md"), "{subject:?}");
                assert!(p.contains("re-read"), "{subject:?}");
                assert!(p.contains("unrelated"), "{subject:?}");
                assert!(p.contains("may read related code"), "{subject:?}");
                assert!(p.contains("make it better"), "{subject:?}");
                assert!(!p.contains("{path}") && !p.contains("{brief}"));
                let lower = p.to_lowercase();
                for banned in [
                    "has been created",
                    "skeleton",
                    "just created",
                    "has been started",
                ] {
                    assert!(!lower.contains(banned), "{subject:?} {banned}");
                }
                assert_eq!(p.contains("clarify their intent"), interview);
                assert_eq!(p.contains("Amendment scope"), amend);
            }
        }
    }

    #[test]
    fn ordinary_without_brief_asks_user() {
        let p = render_edit_prompt(&req(EditSubject::Skill));
        assert!(p.contains("asking the user what they would like to change"));
        assert!(!p.contains("Change brief"));
    }

    #[test]
    fn interview_mentions_intent_constraints_acceptance() {
        let mut r = req(EditSubject::Skill);
        r.interview = true;
        let p = render_edit_prompt(&r);
        assert!(p.contains("intent"));
        assert!(p.contains("constraints"));
        assert!(p.contains("acceptance criteria"));
        assert!(p.contains("Before you edit anything"));
    }

    #[test]
    fn interview_amend_discusses_then_reconciles() {
        let mut r = req(EditSubject::Spec { title: "T" });
        r.interview = true;
        r.amend = true;
        let p = render_edit_prompt(&r);
        assert!(p.contains("Discuss every uncertain finding"));
        assert!(p.contains("reconcile"));
    }

    #[test]
    fn amend_is_kind_specific() {
        let mut r = req(EditSubject::Spec { title: "T" });
        r.amend = true;
        let p = render_edit_prompt(&r);
        assert!(p.contains("completion evidence"));
        assert!(p.contains("without claiming"));

        let mut r = req(EditSubject::Workflow { format: "YAML" });
        r.amend = true;
        let p = render_edit_prompt(&r);
        assert!(p.contains("scripts, commands, paths"));
        assert!(p.contains("existing YAML format"));

        let mut r = req(EditSubject::Skill);
        r.amend = true;
        assert!(render_edit_prompt(&r).contains("repository conventions, APIs"));

        let mut r = req(EditSubject::Agent);
        r.amend = true;
        let p = render_edit_prompt(&r);
        assert!(p.contains("Dockerfile"));
        assert!(p.contains("Never build, install or run the agent"));
    }

    #[test]
    fn amend_common_rules_and_headless_variant() {
        let mut r = req(EditSubject::Agent);
        r.amend = true;
        let interactive = render_edit_prompt(&r);
        assert!(interactive.contains("verified facts, intended future behaviour and unknowns"));
        assert!(interactive.contains("Preserve unresolved requirements"));
        assert!(interactive.contains("never justifies inventing facts"));
        assert!(interactive.contains("ask the user"));
        assert!(
            interactive.contains("never add spec-only")
                || interactive.contains("Never add spec-only")
        );
        r.headless = true;
        let headless = render_edit_prompt(&r);
        assert!(headless.contains("report the limitation and make no speculative change"));
        assert!(!headless.contains("ask the user instead"));
    }

    #[test]
    fn companions_bundle_global_and_location() {
        let companions = vec!["/workspace/a/Dockerfile".to_string()];
        let mut r = req(EditSubject::Agent);
        r.companion_paths = &companions;
        r.global = true;
        r.location = EditLocation::StagedCopy;
        let p = render_edit_prompt(&r);
        assert!(p.contains("- /workspace/a/Dockerfile"));
        assert!(p.contains("working copy"));
        assert!(p.contains("Keep it generic"));

        let mut r = req(EditSubject::Skill);
        r.bundle_dir = Some("/workspace/s");
        let p = render_edit_prompt(&r);
        assert!(p.contains("directory /workspace/s"));

        let mut r = req(EditSubject::Spec { title: "T" });
        r.location = EditLocation::RemoteDraft;
        let p = render_edit_prompt(&r);
        assert!(p.contains("uploads the draft"));
        assert!(p.contains("Do not add a `# <title>` heading"));
    }

    #[test]
    fn single_pass_substitution() {
        let tricky = "{path} {{work_item}} {brief} {title}";
        let mut r = req(EditSubject::Spec { title: tricky });
        r.brief = Some(tricky);
        r.amend = true;
        let p = render_edit_prompt(&r);
        assert!(p.matches(tricky).count() >= 2);
        assert!(p.contains("{{work_item}}"));
        assert!(!p.contains("/workspace/x/main.md {{work_item}}"));
    }

    #[test]
    fn blank_brief_is_ignored() {
        let mut r = req(EditSubject::Skill);
        r.brief = Some("   ");
        assert!(!render_edit_prompt(&r).contains("Change brief"));
    }

    #[test]
    fn brief_narrows_amend_scope_without_being_evidence() {
        let mut r = req(EditSubject::Skill);
        r.amend = true;
        r.brief = Some("only the examples");
        let p = render_edit_prompt(&r);
        assert!(p.contains("not evidence"));
        assert!(p.contains("only the examples"));
    }
}
