//! Authoring prompts for `awman new|edit spec|workflow|skill|agent`.
//!
//! Creation completes a seed; editing changes an existing artifact. Every prompt is built in one pass: static
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
    pub creating: bool,
    pub amend: bool,
    pub headless: bool,
    /// `--prompt` text; untrusted.
    pub brief: Option<&'a str>,
}

/// Build the editing prompt for `request`.
pub fn render_edit_prompt(request: &EditPromptRequest<'_>) -> String {
    let brief = request.brief.map(str::trim).filter(|b| !b.is_empty());
    let mut parts: Vec<String> = Vec::new();

    if request.creating {
        parts.push("Create the artifact by completing the seed file named below. It is not finished merely because a file exists.".into());
    }
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
    parts.push(format_guidance(request.subject).to_string());
    parts.push(boundaries_text(request).to_string());
    parts.push("When finished, summarize the changes and any unresolved questions or validation limitations. Do not claim checks you did not run.".into());
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
    if request.creating {
        return match request.subject {
            EditSubject::Spec { .. } => "Create the work item \"{title}\" in the seed file at {path}. This is the only file you may edit.",
            EditSubject::Workflow { .. } => "Create the {format} workflow \"{name}\" in the seed file at {path}. This is the only file you may edit.",
            EditSubject::Skill => "Create the skill \"{name}\". Its SKILL.md seed is at {path}.",
            EditSubject::Agent => "Create the custom agent definition \"{name}\". Its TOML seed is at {path}.",
        };
    }
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
    let task = if request.amend {
        "Review the artifact against the current repository and available evidence, then amend it to be accurate."
    } else if request.creating {
        "Complete the artifact from the user's title, type and summary."
    } else if has_brief {
        "Apply the change described in the change brief below."
    } else {
        "Start by asking the user what they would like to change."
    };
    let interaction = if request.headless {
        "This is a headless session: do not wait for answers. Use the supplied brief and verifiable context. If essential information is missing, report the limitation and make no speculative change."
    } else {
        "Clarify intent, constraints and acceptance criteria with the user when needed. Ask a few focused questions at a time and wait for the answers before making uncertain changes. The initial summary is a starting point for this conversation."
    };
    format!("{task}\n\n{interaction}")
}

/// Authoring guidance follows the shipped parsers and the Agent Skills specification.
fn format_guidance(subject: EditSubject<'_>) -> &'static str {
    match subject {
        EditSubject::Spec { .. } => "Follow the work-item template already in the file and the repository's AGENTS.md and relevant specifications. Describe the problem, user outcomes, acceptance criteria, implementation plan, edge cases and a proportionate test plan. Read related code to ground the plan. Distinguish verified behavior from intended work and unknowns; do not implement the feature or mark unverified work complete.",
        EditSubject::Workflow { .. } => "Keep the workflow in {format}. awman TOML uses [[step]] entries; YAML uses a steps list. Each step requires a unique name and a non-empty prompt. depends_on names other steps and must form an acyclic graph. Optional step fields are agent, model, overlays, abort_on_failure and read_only_workspace. Omit agent/model overrides unless the user needs them; never invent available agents or models. Top-level fields include title, agent, model, overlays, setup, teardown and teardown_on_failure. Setup/teardown entries use a type discriminator (for example run_shell with command, or run_script with path); they do not accept agent-step prompts or skill overlays. Use only supported operations and verify referenced paths and commands. Keep prompts actionable and dependencies explicit. Consult existing workflows and available repository documentation for more complex setup/teardown operations. Do not run the workflow, commit, push or create a pull request while authoring it.",
        EditSubject::Skill => "Use the Agent Skills format (https://agentskills.io/specification): SKILL.md contains YAML frontmatter followed by Markdown. Required name is {name}, the original skill directory name (a staged directory may have another name): 1–64 lowercase letters/digits/hyphens, no leading/trailing or consecutive hyphens. Required description is non-empty, at most 1024 characters, and explains what the skill does and when to use it. Write focused, actionable instructions, examples and failure handling. Keep SKILL.md concise; place detailed references, scripts or assets inside the allowed skill directory and link them relative to that directory. Do not execute the skill's operational actions just to author its instructions.",
        EditSubject::Agent => concat!("Use awman's custom agent definition schema 1. Unknown fields are rejected. Keep argv as arrays, choose a supported prompt delivery mode, and specify either image.install or image.dockerfile. Never build, install, run or approve this definition; the user reviews it with awman ready afterwards. Only reference a Dockerfile when it is among the allowed companion files. Do not include credentials. The current schema reference is this commented template:\n\n", include_str!("../../../../templates/agent-definition.toml")),
    }
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
not implement features that the item describes and do not change unrelated code or files."
        }
        _ => {
            "Only edit the file at {path}. Preserve all content that is unrelated to the change. \
The user may be editing the same file at the same time, so re-read it immediately before each \
change you make. You may read related code to understand the change, but do not implement \
features that the item describes and do not change unrelated code or files."
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
            creating: false,
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
            for (headless, amend) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut r = req(subject);
                r.amend = amend;
                r.headless = headless;
                r.brief = Some("make it better");
                let p = render_edit_prompt(&r);
                assert!(p.contains("/workspace/x/main.md"), "{subject:?}");
                assert!(p.contains("re-read"), "{subject:?}");
                assert!(p.contains("unrelated"), "{subject:?}");
                assert!(p.contains("may read related code"), "{subject:?}");
                assert!(p.contains("make it better"), "{subject:?}");
                assert!(!p.contains("{path}") && !p.contains("{brief}"));
                assert_eq!(p.contains("do not wait for answers"), headless);
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
    fn interactive_session_clarifies_intent_constraints_acceptance() {
        let r = req(EditSubject::Skill);
        let p = render_edit_prompt(&r);
        assert!(p.contains("intent"));
        assert!(p.contains("constraints"));
        assert!(p.contains("acceptance criteria"));
        assert!(p.contains("wait for the answers"));
    }

    #[test]
    fn interactive_amend_uses_evidence_and_questions() {
        let mut r = req(EditSubject::Spec { title: "T" });
        r.amend = true;
        let p = render_edit_prompt(&r);
        assert!(p.contains("available evidence"));
        assert!(p.contains("wait for the answers"));
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
