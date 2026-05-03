//! Workflow file definitions and parsing — Layer 0.
//!
//! Defines the canonical `Workflow` and `WorkflowStep` data types and supports
//! parsing from Markdown, TOML, and YAML files. Parsing produces serializable
//! data only — no engine logic, no DAG validation (see `workflow_dag.rs`),
//! no execution state (see `workflow_state.rs`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// Supported workflow file formats, detected by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowFormat {
    Markdown,
    Toml,
    Yaml,
}

/// Detect the workflow format from a file extension. `.json` is explicitly
/// rejected — workflows are authored in markdown, TOML, or YAML only.
pub fn detect_format(path: &Path) -> Result<WorkflowFormat, DataError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") => Ok(WorkflowFormat::Markdown),
        Some("toml") => Ok(WorkflowFormat::Toml),
        Some("yml") | Some("yaml") => Ok(WorkflowFormat::Yaml),
        Some(other) => Err(DataError::WorkflowState(format!(
            "unsupported workflow format '.{other}': expected .md, .toml, .yml, or .yaml"
        ))),
        None => Err(DataError::WorkflowState(
            "workflow file has no extension; expected .md, .toml, .yml, or .yaml".into(),
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
}

impl Workflow {
    /// Parse a workflow file's *content* given the resolved format.
    pub fn parse(content: &str, format: WorkflowFormat) -> Result<Self, DataError> {
        match format {
            WorkflowFormat::Markdown => parse_markdown(content),
            WorkflowFormat::Toml => parse_toml(content),
            WorkflowFormat::Yaml => parse_yaml(content),
        }
    }

    /// Convenience: read and parse a workflow from disk.
    pub fn load(path: &Path) -> Result<Self, DataError> {
        let format = detect_format(path)?;
        let content = std::fs::read_to_string(path).map_err(|e| DataError::io(path, e))?;
        Self::parse(&content, format)
    }
}

// ─── Markdown parser ────────────────────────────────────────────────────────

fn parse_markdown(content: &str) -> Result<Workflow, DataError> {
    let mut title: Option<String> = None;
    let mut steps: Vec<WorkflowStep> = Vec::new();

    let mut current_name: Option<String> = None;
    let mut current_depends: Vec<String> = Vec::new();
    let mut current_agent: Option<String> = None;
    let mut current_model: Option<String> = None;
    let mut current_body = String::new();
    let mut in_prompt = false;

    for line in content.lines() {
        if line.starts_with("# ") && title.is_none() && current_name.is_none() {
            title = Some(line[2..].trim().to_string());
            continue;
        }
        if let Some(step_name) = line.strip_prefix("## Step:") {
            flush_md(
                &mut steps,
                &mut current_name,
                &mut current_depends,
                &mut current_agent,
                &mut current_model,
                &mut current_body,
                &mut in_prompt,
            );
            current_name = Some(step_name.trim().to_string());
            continue;
        }
        if line.starts_with("## ") && current_name.is_some() {
            flush_md(
                &mut steps,
                &mut current_name,
                &mut current_depends,
                &mut current_agent,
                &mut current_model,
                &mut current_body,
                &mut in_prompt,
            );
            continue;
        }
        if current_name.is_some() {
            let trimmed = line.trim();
            if trimmed.starts_with("Depends-on:") && !in_prompt {
                let deps_str = trimmed["Depends-on:".len()..].trim();
                current_depends = deps_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                continue;
            }
            if trimmed.starts_with("Agent:") && !in_prompt {
                let v = trimmed["Agent:".len()..].trim();
                if !v.is_empty() {
                    current_agent = Some(v.to_string());
                }
                continue;
            }
            if trimmed.starts_with("Model:") && !in_prompt {
                let v = trimmed["Model:".len()..].trim();
                if !v.is_empty() {
                    current_model = Some(v.to_string());
                }
                continue;
            }
            if (trimmed == "Prompt:" || trimmed.starts_with("Prompt: ")) && !in_prompt {
                in_prompt = true;
                let rest = trimmed["Prompt:".len()..].trim();
                if !rest.is_empty() {
                    current_body.push_str(rest);
                    current_body.push('\n');
                }
                continue;
            }
            if in_prompt {
                current_body.push_str(line);
                current_body.push('\n');
            }
        }
    }
    flush_md(
        &mut steps,
        &mut current_name,
        &mut current_depends,
        &mut current_agent,
        &mut current_model,
        &mut current_body,
        &mut in_prompt,
    );

    if steps.is_empty() {
        return Err(DataError::WorkflowState(
            "workflow file contains no steps; define '## Step: <name>' headings".into(),
        ));
    }

    Ok(Workflow {
        title,
        steps,
        agent: None,
        model: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn flush_md(
    steps: &mut Vec<WorkflowStep>,
    current_name: &mut Option<String>,
    current_depends: &mut Vec<String>,
    current_agent: &mut Option<String>,
    current_model: &mut Option<String>,
    current_body: &mut String,
    in_prompt: &mut bool,
) {
    if let Some(name) = current_name.take() {
        steps.push(WorkflowStep {
            name,
            depends_on: std::mem::take(current_depends),
            prompt_template: std::mem::take(current_body).trim_end().to_string(),
            agent: current_agent.take(),
            model: current_model.take(),
        });
    }
    *in_prompt = false;
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlWorkflow {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(rename = "step", default)]
    steps: Vec<RawStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlWorkflow {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    steps: Vec<RawStep>,
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
    Ok(Workflow {
        title: parsed.title,
        agent: parsed.agent,
        model: parsed.model,
        steps: raw_to_steps(parsed.steps)?,
    })
}

fn parse_yaml(content: &str) -> Result<Workflow, DataError> {
    let stripped = strip_bom(content);
    let parsed: YamlWorkflow = serde_yaml::from_str(stripped)
        .map_err(|e| DataError::WorkflowState(format!("yaml: {e}")))?;
    Ok(Workflow {
        title: parsed.title,
        agent: parsed.agent,
        model: parsed.model,
        steps: raw_to_steps(parsed.steps)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_format_md_toml_yaml() {
        assert_eq!(
            detect_format(&PathBuf::from("a.md")).unwrap(),
            WorkflowFormat::Markdown
        );
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
    fn parse_markdown_minimal() {
        let md = "# My Workflow\n\n## Step: a\nPrompt: do A\n";
        let wf = Workflow::parse(md, WorkflowFormat::Markdown).unwrap();
        assert_eq!(wf.title.as_deref(), Some("My Workflow"));
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].name, "a");
        assert_eq!(wf.steps[0].prompt_template, "do A");
    }

    #[test]
    fn parse_markdown_empty_errors() {
        assert!(Workflow::parse("", WorkflowFormat::Markdown).is_err());
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
    }

    #[test]
    fn parse_yaml_steps() {
        let yaml = "title: Hi\nsteps:\n  - name: a\n    prompt: do A\n";
        let wf = Workflow::parse(yaml, WorkflowFormat::Yaml).unwrap();
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].name, "a");
    }
}
