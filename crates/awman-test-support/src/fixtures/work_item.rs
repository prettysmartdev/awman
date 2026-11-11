//! Work-item markdown following `aspec/work-items/0000-template.md`.

use std::path::{Path, PathBuf};

/// Builds `aspec/work-items/<NNNN>-<slug>.md` text.
#[derive(Debug, Clone)]
pub struct WorkItemBuilder {
    number: u32,
    kind: String,
    title: String,
    issue: String,
    summary: Vec<String>,
    stories: Vec<(String, String, String)>,
    details: Vec<String>,
    edge_cases: Vec<String>,
    tests: Vec<String>,
}

impl WorkItemBuilder {
    /// A `Feature` work item.
    pub fn new(number: u32, title: &str) -> WorkItemBuilder {
        WorkItemBuilder {
            number,
            kind: "Feature".into(),
            title: title.into(),
            issue: String::new(),
            summary: Vec::new(),
            stories: Vec::new(),
            details: Vec::new(),
            edge_cases: Vec::new(),
            tests: Vec::new(),
        }
    }

    /// `Feature`, `Bug` or `Task`.
    pub fn kind(mut self, kind: &str) -> Self {
        self.kind = kind.into();
        self
    }

    /// Issue link.
    pub fn issue(mut self, link: &str) -> Self {
        self.issue = link.into();
        self
    }

    /// A summary bullet.
    pub fn summary(mut self, line: &str) -> Self {
        self.summary.push(line.into());
        self
    }

    /// A user story.
    pub fn story(mut self, as_a: &str, want: &str, so_that: &str) -> Self {
        self.stories
            .push((as_a.into(), want.into(), so_that.into()));
        self
    }

    /// An implementation-details bullet.
    pub fn detail(mut self, line: &str) -> Self {
        self.details.push(line.into());
        self
    }

    /// An edge-case bullet.
    pub fn edge_case(mut self, line: &str) -> Self {
        self.edge_cases.push(line.into());
        self
    }

    /// A test-considerations bullet.
    pub fn test_consideration(mut self, line: &str) -> Self {
        self.tests.push(line.into());
        self
    }

    /// `0042-some-title.md`.
    pub fn file_name(&self) -> String {
        let slug: String = self
            .title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        format!("{:04}-{slug}.md", self.number)
    }

    /// The markdown text.
    pub fn render(&self) -> String {
        let bullets = |items: &[String]| -> String {
            if items.is_empty() {
                "- none\n".to_string()
            } else {
                items.iter().map(|i| format!("- {i}\n")).collect()
            }
        };
        let mut out = format!(
            "# Work Item: {}\n\nTitle: {}\nIssue: {}\n\n## Summary:\n{}\n## User Stories\n\n",
            self.kind,
            self.title,
            self.issue,
            bullets(&self.summary)
        );
        for (i, (as_a, want, so_that)) in self.stories.iter().enumerate() {
            out.push_str(&format!(
                "### User Story {}:\nAs a: {as_a}\n\nI want to:\n{want}\n\nSo I can:\n{so_that}\n\n",
                i + 1
            ));
        }
        out.push_str(&format!(
            "\n## Implementation Details:\n{}\n\n## Edge Case Considerations:\n{}\n## Test Considerations:\n{}\n## Codebase Integration:\n- follow established conventions\n",
            bullets(&self.details),
            bullets(&self.edge_cases),
            bullets(&self.tests)
        ));
        out
    }

    /// Write into `<repo_root>/aspec/work-items/`; returns the path.
    pub fn write(&self, repo_root: &Path) -> PathBuf {
        let dir = repo_root.join("aspec/work-items");
        std::fs::create_dir_all(&dir).expect("create work-items dir");
        let path = dir.join(self.file_name());
        std::fs::write(&path, self.render()).expect("write work item");
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_template_sections() {
        let item = WorkItemBuilder::new(42, "Add a Thing!")
            .kind("Bug")
            .summary("s")
            .story("user", "do x", "get y");
        assert_eq!(item.file_name(), "0042-add-a-thing.md");
        let text = item.render();
        for heading in [
            "# Work Item: Bug",
            "Title: Add a Thing!",
            "## Summary:",
            "### User Story 1:",
            "## Implementation Details:",
            "## Edge Case Considerations:",
            "## Test Considerations:",
        ] {
            assert!(text.contains(heading), "{heading}");
        }
        assert_eq!(text, item.render());
    }
}
