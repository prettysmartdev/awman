//! Typed `ContextPromptBuilder` — assembles the combined system-prompt text
//! for context overlays. Pure text construction; no OS or network access.

/// Live workflow state for the dynamic `context(workflow)` prompt.
#[derive(Debug, Clone)]
pub struct WorkflowStepInfo {
    pub workflow_title: String,
    pub current_step_name: String,
    pub current_step_index: usize,
    pub total_steps: usize,
    pub steps: Vec<(String, WorkflowStepState)>,
    pub work_item_number: Option<u32>,
    pub work_item_title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStepState {
    Completed,
    InProgress,
    Pending,
}

/// Accumulates the per-scope sections active for one agent run and renders the
/// single combined system-prompt string (sections separated by `\n---\n`,
/// workflow section always last).
#[derive(Debug, Default)]
pub struct ContextPromptBuilder {
    global: bool,
    repo: bool,
    workflow: Option<WorkflowSection>,
}

#[derive(Debug)]
enum WorkflowSection {
    Full(WorkflowStepInfo),
    Oneshot,
}

const GLOBAL_PROMPT: &str = "\
## Global Developer Context

You have access to a persistent global context directory mounted at /awman/context/global.

This directory is the developer's personal, cross-project workspace — maintained by them and shared across all agents, projects, and workflows they run with awman. It is meant to be portable and durable: a personalized addition to per-project CLAUDE.md files that travels with this developer everywhere.

The directory may contain:
- Personal coding style preferences and conventions the developer always wants followed
- Notes on recurring mistakes, things to avoid, or common gotchas encountered across projects
- Shared tools, scripts, or templates that may be used freely
- Any standing guidance the developer wants applied to all of their work

Instructions:
1. Read all files in /awman/context/global at the start of your task to understand the developer's preferences and any standing guidance they have left.
2. You SHOULD write to files inside /awman/context/global to record any significant insights or mistakes, or leave guidance you want future agent sessions to know based on your interactions with the developer. The developer will review and curate these files over time.
3. Treat the contents of this directory as extremely valuable developer guidance and context — refer to it throughout your work.";

const REPO_PROMPT: &str = "\
## Repository-Specific Context

You have access to a repository-specific context directory mounted at /awman/context/repo.

This directory contains knowledge and guidance specific to the project you are currently working in. It is maintained collaboratively by the developer and agents who have worked on this codebase before.

The directory may contain:
- Architecture notes and key design decisions
- Project-specific conventions, patterns, and best practices
- Known gotchas, workarounds, and areas of technical debt
- Domain knowledge and business logic documentation
- Notes from previous agent sessions working on this codebase

Instructions:
1. Read all files in /awman/context/repo at the start of your task to orient yourself to the project-specific context.
2. You SHOULD write to files in /awman/context/repo to capture any significant insights discovered during your work, document decisions made, or leave guidance for future agent sessions. The developer will review and curate these files.
3. Treat the contents of this directory as extremely valuable project context — refer to it alongside the codebase itself throughout your work.";

const WORKFLOW_ONESHOT_PROMPT: &str = "\
## Workflow Context

You are running a one-shot awman task. A shared workflow context directory is available at /awman/context/workflow if you need to persist state.";

impl ContextPromptBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_global(mut self) -> Self {
        self.global = true;
        self
    }

    pub fn with_repo(mut self) -> Self {
        self.repo = true;
        self
    }

    pub fn with_workflow(mut self, info: &WorkflowStepInfo) -> Self {
        self.workflow = Some(WorkflowSection::Full(info.clone()));
        self
    }

    pub fn with_workflow_oneshot(mut self) -> Self {
        self.workflow = Some(WorkflowSection::Oneshot);
        self
    }

    /// `None` when no scopes were added.
    pub fn build(self) -> Option<String> {
        let mut sections: Vec<String> = Vec::new();

        if self.global {
            sections.push(GLOBAL_PROMPT.to_string());
        }
        if self.repo {
            sections.push(REPO_PROMPT.to_string());
        }

        match self.workflow {
            Some(WorkflowSection::Full(info)) => {
                sections.push(render_workflow_prompt(&info));
            }
            Some(WorkflowSection::Oneshot) => {
                sections.push(WORKFLOW_ONESHOT_PROMPT.to_string());
            }
            None => {}
        }

        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n---\n"))
        }
    }
}

/// Who an outputs-directory prompt line is for (WI 0137 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputsPromptRole<'a> {
    /// The `exec prompt` agent.
    Prompt,
    /// The dynamic-workflow leader (and its repair runs).
    Leader,
    /// A workflow step, by name: parallel steps share the one directory.
    Step(&'a str),
}

/// The one line naming `/awman/output` as the place for machine-readable
/// results. Only added when the command really mounts the directory.
pub fn outputs_prompt_line(role: OutputsPromptRole<'_>) -> String {
    const BASE: &str = "Write machine-readable results (JSON, reports, generated files) for \
                        the caller under /awman/output; awman lists and serves every file there.";
    match role {
        OutputsPromptRole::Prompt | OutputsPromptRole::Leader => BASE.to_string(),
        OutputsPromptRole::Step(name) => format!(
            "{BASE} Other steps of this workflow share the directory: when steps run in \
             parallel, write under /awman/output/{name}/ so files do not collide."
        ),
    }
}

/// Append the outputs section to a rendered system prompt (or start one),
/// after every context section.
pub fn with_outputs_section(system_prompt: Option<String>, role: OutputsPromptRole<'_>) -> String {
    let section = format!("## Outputs\n\n{}", outputs_prompt_line(role));
    match system_prompt {
        Some(prompt) if !prompt.is_empty() => format!("{prompt}\n---\n{section}"),
        _ => section,
    }
}

fn render_workflow_prompt(info: &WorkflowStepInfo) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "## Workflow Context\n\n\
         You are running as part of the multi-agent \"{}\" workflow, managed by awman.\n\n\
         Your current step: {} (step {} of {})\n\n\
         Workflow progress:\n",
        info.workflow_title,
        info.current_step_name,
        info.current_step_index + 1,
        info.total_steps,
    ));

    for (name, state) in &info.steps {
        let marker = match state {
            WorkflowStepState::Completed => "[✓]",
            WorkflowStepState::InProgress => "[→]",
            WorkflowStepState::Pending => "[○]",
        };
        let label = match state {
            WorkflowStepState::Completed => "completed",
            WorkflowStepState::InProgress => "in progress (this is you)",
            WorkflowStepState::Pending => "pending",
        };
        out.push_str(&format!("  {marker} {name} — {label}\n"));
    }

    if let Some(wi_num) = info.work_item_number {
        out.push('\n');
        if let Some(ref title) = info.work_item_title {
            out.push_str(&format!("Work item: #{wi_num}\n{title}\n"));
        } else {
            out.push_str(&format!("Work item: #{wi_num}\n"));
        }
    }

    out.push_str(
        "\nYou have access to a shared workflow context directory mounted at /awman/context/workflow. \
         Every agent step in this workflow shares this directory and can read and write files there.\n\n\
         Instructions:\n\
         1. At the start of your task, read any files left by previous steps in /awman/context/workflow — \
         they may contain intermediate results, shared state, helpful scripts, or instructions from earlier steps.\n\
         2. Write your outputs, notes, intermediate artifacts, scripts, investigation results, and any state \
         that later steps will need into /awman/context/workflow. Use descriptive file names so downstream \
         steps understand what you produced.\n\
         3. You are one step in a coordinated multi-agent workflow. Produce your deliverables reliably \
         (no more, no less), document what you produced in the provided directory, and leave the workspace \
         ready for the next step.",
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_line_names_the_mount_for_every_role() {
        for role in [
            OutputsPromptRole::Prompt,
            OutputsPromptRole::Leader,
            OutputsPromptRole::Step("tests"),
        ] {
            assert!(outputs_prompt_line(role).contains("/awman/output"));
            assert!(!outputs_prompt_line(role).contains('\n'));
        }
        assert!(
            outputs_prompt_line(OutputsPromptRole::Step("tests")).contains("/awman/output/tests/")
        );
        assert!(!outputs_prompt_line(OutputsPromptRole::Prompt).contains("parallel"));
    }

    #[test]
    fn outputs_section_is_appended_last_or_stands_alone() {
        let alone = with_outputs_section(None, OutputsPromptRole::Prompt);
        assert!(alone.starts_with("## Outputs"));
        let combined = with_outputs_section(
            ContextPromptBuilder::new().with_repo().build(),
            OutputsPromptRole::Leader,
        );
        assert!(combined.starts_with("## Repository-Specific Context"));
        assert!(combined.ends_with(&outputs_prompt_line(OutputsPromptRole::Leader)));
        assert!(combined.contains("\n---\n## Outputs"));
    }

    #[test]
    fn empty_builder_returns_none() {
        assert!(ContextPromptBuilder::new().build().is_none());
    }

    #[test]
    fn global_only_contains_global_path() {
        let prompt = ContextPromptBuilder::new().with_global().build().unwrap();
        assert!(prompt.contains("/awman/context/global"));
        assert!(!prompt.contains("/awman/context/repo"));
        assert!(!prompt.contains("/awman/context/workflow"));
    }

    #[test]
    fn repo_only_contains_repo_path() {
        let prompt = ContextPromptBuilder::new().with_repo().build().unwrap();
        assert!(prompt.contains("/awman/context/repo"));
        assert!(!prompt.contains("/awman/context/global"));
    }

    #[test]
    fn workflow_oneshot_contains_oneshot_text() {
        let prompt = ContextPromptBuilder::new()
            .with_workflow_oneshot()
            .build()
            .unwrap();
        assert!(prompt.contains("one-shot"));
        assert!(prompt.contains("/awman/context/workflow"));
    }

    #[test]
    fn workflow_full_contains_step_markers() {
        let info = WorkflowStepInfo {
            workflow_title: "My Workflow".to_string(),
            current_step_name: "step-b".to_string(),
            current_step_index: 1,
            total_steps: 3,
            steps: vec![
                ("step-a".to_string(), WorkflowStepState::Completed),
                ("step-b".to_string(), WorkflowStepState::InProgress),
                ("step-c".to_string(), WorkflowStepState::Pending),
            ],
            work_item_number: None,
            work_item_title: None,
        };
        let prompt = ContextPromptBuilder::new()
            .with_workflow(&info)
            .build()
            .unwrap();
        assert!(prompt.contains("My Workflow"));
        assert!(prompt.contains("[✓] step-a"));
        assert!(prompt.contains("[→] step-b"));
        assert!(prompt.contains("[○] step-c"));
        assert!(prompt.contains("step 2 of 3"));
    }

    #[test]
    fn combined_scopes_joined_by_separator_workflow_last() {
        let info = WorkflowStepInfo {
            workflow_title: "T".to_string(),
            current_step_name: "s".to_string(),
            current_step_index: 0,
            total_steps: 1,
            steps: vec![("s".to_string(), WorkflowStepState::InProgress)],
            work_item_number: None,
            work_item_title: None,
        };
        let prompt = ContextPromptBuilder::new()
            .with_global()
            .with_repo()
            .with_workflow(&info)
            .build()
            .unwrap();
        let parts: Vec<&str> = prompt.split("\n---\n").collect();
        assert_eq!(parts.len(), 3, "expected 3 sections; got {}", parts.len());
        assert!(parts[0].contains("Global Developer Context"));
        assert!(parts[1].contains("Repository-Specific Context"));
        assert!(parts[2].contains("Workflow Context"));
    }
}
