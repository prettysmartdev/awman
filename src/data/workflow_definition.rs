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

/// Configuration for automatic remediation when a step fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemediationConfig {
    pub prompt: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub max_attempts: u32,
}

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

/// How a `clone_repo` setup step handles a target directory that already
/// holds a clone of the requested URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneConflictMode {
    /// Leave the existing clone in place and succeed (default).
    #[default]
    Skip,
    /// Delete the existing clone and clone a fresh copy.
    Replace,
    /// Fail the step.
    Error,
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
        #[serde(default)]
        conflict_mode: CloneConflictMode,
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
    PollCi {
        #[serde(default)]
        interval_secs: Option<u32>,
        #[serde(default)]
        max_retries: Option<u32>,
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
        /// Stage changes according to Git's ignore rules before committing.
        /// Defaults to `true` so a teardown never leaves the agent's work out.
        #[serde(default = "default_add_all")]
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
    PollCi {
        #[serde(default)]
        interval_secs: Option<u32>,
        #[serde(default)]
        max_retries: Option<u32>,
    },
}

fn default_add_all() -> bool {
    true
}

/// A setup step entry with optional per-step overlays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStepEntry {
    #[serde(default)]
    pub overlays: Option<Vec<String>>,
    #[serde(default)]
    pub abort_on_failure: bool,
    #[serde(default)]
    pub on_failure: Option<RemediationConfig>,
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
    #[serde(default)]
    pub on_failure: Option<RemediationConfig>,
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
    /// Overlays applied to every agent step in this workflow.
    #[serde(default)]
    pub overlays: Option<Vec<String>>,
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

fn validate_remediation_configs(wf: &Workflow) -> Result<(), DataError> {
    for (i, entry) in wf.setup.iter().enumerate() {
        if let Some(ref rem) = entry.on_failure {
            if rem.max_attempts == 0 {
                return Err(DataError::WorkflowState(format!(
                    "setup step {i}: on_failure.max_attempts must be >= 1, got 0"
                )));
            }
        }
    }
    for (i, entry) in wf.teardown.iter().enumerate() {
        if let Some(ref rem) = entry.on_failure {
            if rem.max_attempts == 0 {
                return Err(DataError::WorkflowState(format!(
                    "teardown step {i}: on_failure.max_attempts must be >= 1, got 0"
                )));
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
        validate_remediation_configs(&wf)?;
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
    #[serde(default)]
    overlays: Option<Vec<String>>,
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
    overlays: Option<Vec<String>>,
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
        overlays: parsed.overlays,
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
        overlays: parsed.overlays,
        steps: raw_to_steps(parsed.steps)?,
        setup: parsed.setup,
        teardown: parsed.teardown,
        teardown_on_failure: parsed.teardown_on_failure,
    })
}
