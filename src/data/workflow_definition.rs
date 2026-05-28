//! Workflow file definitions and parsing — Layer 0.
//!
//! Defines the canonical `Workflow` and `WorkflowStep` data types and supports
//! parsing from TOML and YAML files. Parsing produces serializable
//! data only — no engine logic, no DAG validation (see `workflow_dag.rs`),
//! no execution state (see `workflow_state.rs`).

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// Supported workflow file formats, detected by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowFormat {
    Toml,
    Yaml,
}

/// Detect the workflow format from a file extension. `.md` files return
/// `MarkdownNoLongerSupported`; `.json` is explicitly rejected.
pub fn detect_format(path: &Path) -> Result<WorkflowFormat, DataError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") => Err(DataError::MarkdownNoLongerSupported {
            path: path.to_path_buf(),
        }),
        Some("toml") => Ok(WorkflowFormat::Toml),
        Some("yml") | Some("yaml") => Ok(WorkflowFormat::Yaml),
        Some(other) => Err(DataError::WorkflowState(format!(
            "unsupported workflow format '.{other}': expected .toml, .yml, or .yaml"
        ))),
        None => Err(DataError::WorkflowState(
            "workflow file has no extension; expected .toml, .yml, or .yaml".into(),
        )),
    }
}

/// A single step in a multi-agent workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub prompt_template: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub overlays: Option<Vec<String>>,
    #[serde(default)]
    pub abort_on_failure: bool,
}

/// A setup phase step — executed before the main workflow steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SetupStep {
    CloneRepo {
        url: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        into: Option<String>,
    },
    CheckoutCreateBranch {
        branch: String,
        #[serde(default)]
        base: Option<String>,
    },
    PullBranch {
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        branch: Option<String>,
    },
    RunShell {
        command: String,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    RunScript {
        path: String,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
}

/// A teardown phase step — executed after the main workflow steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeardownStep {
    RunShell {
        command: String,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    RunScript {
        path: String,
    },
    CommitChanges {
        message: String,
        #[serde(default)]
        add_all: bool,
    },
    CreatePullRequest {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        base: Option<String>,
    },
    PushBranch {
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        branch: Option<String>,
    },
}

/// A setup step entry with optional per-step overlays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStepEntry {
    #[serde(default)]
    pub overlays: Option<Vec<String>>,
    #[serde(default)]
    pub abort_on_failure: bool,
    #[serde(flatten)]
    pub step: SetupStep,
}

/// A teardown step entry with optional per-step overlays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeardownStepEntry {
    #[serde(default)]
    pub overlays: Option<Vec<String>>,
    #[serde(default)]
    pub abort_on_failure: bool,
    #[serde(flatten)]
    pub step: TeardownStep,
}

/// Parsed, validated workflow definition. The DAG (`workflow_dag.rs`) and
/// runtime state (`workflow_state.rs`) live in separate modules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub title: Option<String>,
    pub steps: Vec<WorkflowStep>,
    /// Optional workflow-level default agent (overridden by step-level `agent`).
    #[serde(default)]
    pub agent: Option<String>,
    /// Optional workflow-level default model (overridden by step-level `model`).
    #[serde(default)]
    pub model: Option<String>,
    /// Setup steps run before the first workflow step.
    #[serde(default)]
    pub setup: Vec<SetupStepEntry>,
    /// Teardown steps run after the last workflow step (or on failure, if configured).
    #[serde(default)]
    pub teardown: Vec<TeardownStepEntry>,
    /// If true, teardown runs even when the workflow fails.
    #[serde(default)]
    pub teardown_on_failure: bool,
}

/// Validate that no setup or teardown step's overlay list contains a `skill(...)`
/// or `skills(...)` expression. Those overlay types require an agent container and
/// are meaningless on host-executed setup/teardown steps.
///
/// `skills(...)` is checked first so its "removed form" error is reported before
/// the generic "skills not valid on setup/teardown" error.
fn validate_setup_teardown_overlays(wf: &Workflow) -> Result<(), DataError> {
    for (i, entry) in wf.setup.iter().enumerate() {
        if let Some(overlays) = &entry.overlays {
            for overlay in overlays {
                let t = overlay.trim();
                if t.starts_with("skills(") {
                    return Err(DataError::WorkflowState(format!(
                        "setup step {i}: '{overlay}': skills() has been removed; \
                         use skill(*) to mount all skills or skill(name) for a specific named skill — \
                         and only on agent (workflow step) entries, not setup steps"
                    )));
                }
                if t.starts_with("skill(") {
                    return Err(DataError::WorkflowState(format!(
                        "setup step {i}: '{overlay}': skill() overlays are only valid on agent \
                         (workflow step) entries; setup steps do not run agent containers"
                    )));
                }
            }
        }
    }
    for (i, entry) in wf.teardown.iter().enumerate() {
        if let Some(overlays) = &entry.overlays {
            for overlay in overlays {
                let t = overlay.trim();
                if t.starts_with("skills(") {
                    return Err(DataError::WorkflowState(format!(
                        "teardown step {i}: '{overlay}': skills() has been removed; \
                         use skill(*) to mount all skills or skill(name) for a specific named skill — \
                         and only on agent (workflow step) entries, not teardown steps"
                    )));
                }
                if t.starts_with("skill(") {
                    return Err(DataError::WorkflowState(format!(
                        "teardown step {i}: '{overlay}': skill() overlays are only valid on agent \
                         (workflow step) entries; teardown steps do not run agent containers"
                    )));
                }
            }
        }
    }
    Ok(())
}

impl Workflow {
    /// Parse a workflow file's *content* given the resolved format.
    pub fn parse(content: &str, format: WorkflowFormat) -> Result<Self, DataError> {
        let wf = match format {
            WorkflowFormat::Toml => parse_toml(content),
            WorkflowFormat::Yaml => parse_yaml(content),
        }?;
        validate_setup_teardown_overlays(&wf)?;
        Ok(wf)
    }

    /// Convenience: read and parse a workflow from disk.
    pub fn load(path: &Path) -> Result<Self, DataError> {
        let format = detect_format(path)?;
        let content = std::fs::read_to_string(path).map_err(|e| DataError::io(path, e))?;
        Self::parse(&content, format)
    }
}

// ─── TOML/YAML parsers ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    name: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    overlays: Option<Vec<String>>,
    #[serde(default)]
    abort_on_failure: bool,
}

#[derive(Debug, Deserialize)]
struct TomlWorkflow {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(rename = "step", alias = "steps", default)]
    steps: Vec<RawStep>,
    #[serde(default)]
    setup: Vec<SetupStepEntry>,
    #[serde(default)]
    teardown: Vec<TeardownStepEntry>,
    #[serde(default)]
    teardown_on_failure: bool,
}

#[derive(Debug, Deserialize)]
struct YamlWorkflow {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    steps: Vec<RawStep>,
    #[serde(default)]
    setup: Vec<SetupStepEntry>,
    #[serde(default)]
    teardown: Vec<TeardownStepEntry>,
    #[serde(default)]
    teardown_on_failure: bool,
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

fn raw_to_steps(raw: Vec<RawStep>) -> Result<Vec<WorkflowStep>, DataError> {
    let mut steps = Vec::with_capacity(raw.len());
    for (idx, r) in raw.into_iter().enumerate() {
        let name = r.name.ok_or_else(|| {
            DataError::WorkflowState(format!("step {idx}: missing required field 'name'"))
        })?;
        let prompt_raw = r.prompt.ok_or_else(|| {
            DataError::WorkflowState(format!("step {idx} ('{name}'): missing required 'prompt'"))
        })?;
        let prompt_template = prompt_raw.trim().to_string();
        steps.push(WorkflowStep {
            name,
            depends_on: r.depends_on,
            prompt_template,
            agent: r.agent,
            model: r.model,
            overlays: r.overlays,
            abort_on_failure: r.abort_on_failure,
        });
    }
    if steps.is_empty() {
        return Err(DataError::WorkflowState(
            "workflow file contains no steps".into(),
        ));
    }
    Ok(steps)
}

fn parse_toml(content: &str) -> Result<Workflow, DataError> {
    let stripped = strip_bom(content);
    let parsed: TomlWorkflow =
        toml::from_str(stripped).map_err(|e| DataError::WorkflowState(format!("toml: {e}")))?;
    let title = parsed.title.or(parsed.name);
    Ok(Workflow {
        title,
        agent: parsed.agent,
        model: parsed.model,
        steps: raw_to_steps(parsed.steps)?,
        setup: parsed.setup,
        teardown: parsed.teardown,
        teardown_on_failure: parsed.teardown_on_failure,
    })
}

fn parse_yaml(content: &str) -> Result<Workflow, DataError> {
    let stripped = strip_bom(content);
    let parsed: YamlWorkflow = serde_yaml::from_str(stripped)
        .map_err(|e| DataError::WorkflowState(format!("yaml: {e}")))?;
    let title = parsed.title.or(parsed.name);
    Ok(Workflow {
        title,
        agent: parsed.agent,
        model: parsed.model,
        steps: raw_to_steps(parsed.steps)?,
        setup: parsed.setup,
        teardown: parsed.teardown,
        teardown_on_failure: parsed.teardown_on_failure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_format_toml_yaml() {
        assert_eq!(
            detect_format(&PathBuf::from("a.toml")).unwrap(),
            WorkflowFormat::Toml
        );
        assert_eq!(
            detect_format(&PathBuf::from("a.yml")).unwrap(),
            WorkflowFormat::Yaml
        );
        assert_eq!(
            detect_format(&PathBuf::from("a.yaml")).unwrap(),
            WorkflowFormat::Yaml
        );
        assert!(detect_format(&PathBuf::from("a.json")).is_err());
        assert!(detect_format(&PathBuf::from("noext")).is_err());
    }

    #[test]
    fn detect_format_md_returns_markdown_no_longer_supported() {
        let err = detect_format(&PathBuf::from("a.md")).unwrap_err();
        assert!(
            matches!(err, DataError::MarkdownNoLongerSupported { .. }),
            "expected MarkdownNoLongerSupported, got: {err:?}"
        );
    }

    #[test]
    fn parse_toml_array_of_step() {
        let toml = r#"
title = "T"
[[step]]
name = "a"
prompt = "do A"

[[step]]
name = "b"
prompt = "do B"
depends_on = ["a"]
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert_eq!(wf.steps.len(), 2);
        assert_eq!(wf.steps[1].depends_on, vec!["a".to_string()]);
        assert!(wf.setup.is_empty());
        assert!(wf.teardown.is_empty());
        assert!(!wf.teardown_on_failure);
    }

    #[test]
    fn parse_yaml_steps() {
        let yaml = "title: Hi\nsteps:\n  - name: a\n    prompt: do A\n";
        let wf = Workflow::parse(yaml, WorkflowFormat::Yaml).unwrap();
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].name, "a");
        assert!(wf.setup.is_empty());
        assert!(wf.teardown.is_empty());
    }

    #[test]
    fn parse_toml_with_setup_and_teardown() {
        let toml = r#"
name = "implement-feature"
teardown_on_failure = true

[[setup]]
type = "checkout_create_branch"
branch = "feature/my-thing"

[[setup]]
type = "run_shell"
command = "cargo fetch"

[[steps]]
name = "implement"
prompt = "Implement the feature described in SPEC.md"

[[teardown]]
type = "run_shell"
command = "cargo test"

[[teardown]]
type = "create_pull_request"
title = "feat: implement my feature"
body = "Automated PR from awman workflow"
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert_eq!(wf.title.as_deref(), Some("implement-feature"));
        assert_eq!(wf.setup.len(), 2);
        assert_eq!(wf.teardown.len(), 2);
        assert!(wf.teardown_on_failure);

        assert!(matches!(
            &wf.setup[0].step,
            SetupStep::CheckoutCreateBranch { branch, .. } if branch == "feature/my-thing"
        ));
        assert!(matches!(
            &wf.setup[1].step,
            SetupStep::RunShell { command, .. } if command == "cargo fetch"
        ));
        assert!(matches!(
            &wf.teardown[0].step,
            TeardownStep::RunShell { command, .. } if command == "cargo test"
        ));
        assert!(matches!(
            &wf.teardown[1].step,
            TeardownStep::CreatePullRequest { title, body, .. }
                if title.as_deref() == Some("feat: implement my feature") && body.as_deref() == Some("Automated PR from awman workflow")
        ));
    }

    #[test]
    fn parse_yaml_with_setup_and_teardown() {
        let yaml = r#"
name: implement-feature
teardown_on_failure: true
setup:
  - type: checkout_create_branch
    branch: feature/my-thing
  - type: run_shell
    command: cargo fetch
steps:
  - name: implement
    prompt: Implement the feature
teardown:
  - type: commit_changes
    message: "auto: implemented feature"
    add_all: true
  - type: push_branch
"#;
        let wf = Workflow::parse(yaml, WorkflowFormat::Yaml).unwrap();
        assert_eq!(wf.setup.len(), 2);
        assert_eq!(wf.teardown.len(), 2);
        assert!(wf.teardown_on_failure);

        assert!(matches!(
            &wf.teardown[0].step,
            TeardownStep::CommitChanges { message, add_all } if message == "auto: implemented feature" && *add_all
        ));
        assert!(matches!(
            &wf.teardown[1].step,
            TeardownStep::PushBranch { remote, branch } if remote.is_none() && branch.is_none()
        ));
    }

    #[test]
    fn existing_workflow_without_setup_teardown_parses() {
        let toml = r#"
[[step]]
name = "a"
prompt = "do A"
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert!(wf.setup.is_empty());
        assert!(wf.teardown.is_empty());
        assert!(!wf.teardown_on_failure);
    }

    // ─── TeardownStepEntry overlays ───────────────────────────────────────────

    #[test]
    fn teardown_push_branch_with_ssh_overlay_deserializes() {
        let toml = r#"
[[steps]]
name = "impl"
prompt = "do work"

[[teardown]]
type = "push_branch"
overlays = ["ssh()"]
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert_eq!(wf.teardown.len(), 1);
        assert!(
            matches!(&wf.teardown[0].step, TeardownStep::PushBranch { .. }),
            "teardown step must be PushBranch; got {:?}",
            wf.teardown[0].step
        );
        assert_eq!(
            wf.teardown[0].overlays,
            Some(vec!["ssh()".to_string()]),
            "ssh() overlay must be preserved in TeardownStepEntry"
        );
    }

    #[test]
    fn teardown_create_pr_with_env_overlay_deserializes() {
        let yaml = r#"
steps:
  - name: impl
    prompt: do work
teardown:
  - type: create_pull_request
    title: "My PR"
    overlays:
      - "env(GITHUB_TOKEN)"
"#;
        let wf = Workflow::parse(yaml, WorkflowFormat::Yaml).unwrap();
        assert_eq!(wf.teardown.len(), 1);
        assert!(
            matches!(&wf.teardown[0].step, TeardownStep::CreatePullRequest { title, .. } if title.as_deref() == Some("My PR")),
            "teardown step must be CreatePullRequest with correct title"
        );
        assert_eq!(
            wf.teardown[0].overlays,
            Some(vec!["env(GITHUB_TOKEN)".to_string()]),
            "env(GITHUB_TOKEN) overlay must be preserved"
        );
    }

    // ─── Setup/teardown overlay validation ───────────────────────────────────

    #[test]
    fn setup_step_with_skill_overlay_is_data_error_at_load_time() {
        let toml = r#"
[[setup]]
type = "run_shell"
command = "echo hello"
overlays = ["skill(*)"]

[[steps]]
name = "impl"
prompt = "do work"
"#;
        let result = Workflow::parse(toml, WorkflowFormat::Toml);
        assert!(
            result.is_err(),
            "skill(*) in setup step must be a DataError"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("skill(") || msg.contains("setup"),
            "error must mention skill or setup step context; got: {msg}"
        );
    }

    #[test]
    fn setup_step_with_skills_plural_overlay_is_error_before_skill_check() {
        // skills() is the removed plural form — its error must fire before the
        // "skill not valid on setup" check, so the message mentions the removed form.
        let toml = r#"
[[setup]]
type = "run_shell"
command = "echo hello"
overlays = ["skills()"]

[[steps]]
name = "impl"
prompt = "do work"
"#;
        let result = Workflow::parse(toml, WorkflowFormat::Toml);
        assert!(
            result.is_err(),
            "skills() in setup step overlays must be an error"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("skills()") || msg.contains("removed"),
            "error must mention the removed skills() form; got: {msg}"
        );
    }

    #[test]
    fn teardown_step_with_skill_overlay_is_data_error() {
        let yaml = r#"
steps:
  - name: impl
    prompt: do work
teardown:
  - type: run_shell
    command: echo done
    overlays:
      - "skill(*)"
"#;
        let result = Workflow::parse(yaml, WorkflowFormat::Yaml);
        assert!(
            result.is_err(),
            "skill(*) in teardown step must be a DataError"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("skill(") || msg.contains("teardown"),
            "error must mention skill or teardown context; got: {msg}"
        );
    }

    #[test]
    fn workflow_step_with_skill_overlay_is_valid() {
        // skill() is valid on agent (workflow) steps — not setup/teardown.
        let toml = r#"
[[steps]]
name = "impl"
prompt = "do work"
overlays = ["skill(*)", "skill(lint)"]
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert_eq!(
            wf.steps[0].overlays,
            Some(vec!["skill(*)".to_string(), "skill(lint)".to_string()])
        );
    }
}
