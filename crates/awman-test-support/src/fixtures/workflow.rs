//! Workflow definition files (`aspec/workflows/<name>.toml`).

use std::path::{Path, PathBuf};

/// One `[[step]]`.
#[derive(Debug, Clone, Default)]
pub struct WorkflowStep {
    /// Step name.
    pub name: String,
    /// Prompt text.
    pub prompt: String,
    /// Steps this one depends on.
    pub depends_on: Vec<String>,
    /// Agent override.
    pub agent: Option<String>,
    /// Model override.
    pub model: Option<String>,
}

/// Builds workflow TOML.
#[derive(Debug, Clone)]
pub struct WorkflowBuilder {
    title: String,
    steps: Vec<WorkflowStep>,
}

impl WorkflowBuilder {
    /// A workflow with a title and no steps.
    pub fn new(title: &str) -> WorkflowBuilder {
        WorkflowBuilder {
            title: title.into(),
            steps: Vec::new(),
        }
    }

    /// Add a step with a prompt and dependencies.
    pub fn step(mut self, name: &str, prompt: &str, depends_on: &[&str]) -> Self {
        self.steps.push(WorkflowStep {
            name: name.into(),
            prompt: prompt.into(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            ..WorkflowStep::default()
        });
        self
    }

    /// Add a fully specified step.
    pub fn step_with(mut self, step: WorkflowStep) -> Self {
        self.steps.push(step);
        self
    }

    /// The TOML text.
    pub fn render(&self) -> String {
        let mut doc = toml::Table::new();
        doc.insert("title".into(), toml::Value::String(self.title.clone()));
        let steps: Vec<toml::Value> = self
            .steps
            .iter()
            .map(|s| {
                let mut t = toml::Table::new();
                t.insert("name".into(), s.name.clone().into());
                if !s.depends_on.is_empty() {
                    t.insert(
                        "depends_on".into(),
                        toml::Value::Array(s.depends_on.iter().cloned().map(Into::into).collect()),
                    );
                }
                if let Some(agent) = &s.agent {
                    t.insert("agent".into(), agent.clone().into());
                }
                if let Some(model) = &s.model {
                    t.insert("model".into(), model.clone().into());
                }
                t.insert("prompt".into(), s.prompt.clone().into());
                toml::Value::Table(t)
            })
            .collect();
        doc.insert("step".into(), toml::Value::Array(steps));
        toml::to_string(&doc).expect("workflow TOML")
    }

    /// Write `<dir>/<name>.toml`; returns the path.
    pub fn write(&self, dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create workflow dir");
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, self.render()).expect("write workflow");
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_parseable_steps() {
        let text = WorkflowBuilder::new("T")
            .step("plan", "Plan it", &[])
            .step("build", "Build it", &["plan"])
            .render();
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(parsed["title"].as_str(), Some("T"));
        let steps = parsed["step"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1]["depends_on"][0].as_str(), Some("plan"));
    }
}
