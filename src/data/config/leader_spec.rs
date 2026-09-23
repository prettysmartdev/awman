//! `LeaderSpec` — an `agent::model` pair.
//!
//! The shape is a data contract: it arrives on `--leader` (a flag), in
//! `dynamicWorkflows.defaultLeader` (a config field), and in a squad task's
//! leader setting. Before WI 0114 F-51 the parse lived in Layer 2
//! (`commands/exec_workflow.rs`) and the config validator in Layer 0 restated
//! the same two-non-empty-components rule beside a comment saying it did so
//! "without depending on the command layer's `LeaderSpec`". One rule, one
//! place, and the layer that owns it is the one that owns the config.

/// Fully-specified leader agent selection parsed from `agent::model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderSpec {
    pub agent: String,
    pub model: String,
}

/// Why an `agent::model` value could not be parsed.
///
/// Carries no message of its own: the two callers word the same problem
/// differently — one names the flag, the other names the config field — and
/// both spellings are part of their interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderSpecError {
    /// Not exactly two `::`-separated components, or one of them was empty.
    Shape,
}

impl LeaderSpec {
    /// Parse `agent::model`: exactly two non-empty components separated by a
    /// single `::`.
    pub fn parse(raw: &str) -> Result<Self, LeaderSpecError> {
        let parts: Vec<&str> = raw.split("::").collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(LeaderSpecError::Shape);
        }
        Ok(LeaderSpec {
            agent: parts[0].to_string(),
            model: parts[1].to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_spec_splits_into_agent_and_model() {
        let spec = LeaderSpec::parse("claude::claude-opus-4-8").unwrap();
        assert_eq!(spec.agent, "claude");
        assert_eq!(spec.model, "claude-opus-4-8");
    }

    #[test]
    fn anything_but_two_non_empty_components_is_a_shape_error() {
        for raw in ["claude", "claude::", "::model", "a::b::c", ""] {
            assert_eq!(
                LeaderSpec::parse(raw),
                Err(LeaderSpecError::Shape),
                "{raw:?} must not parse"
            );
        }
    }
}
