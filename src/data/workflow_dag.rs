//! Validated step-graph for a workflow — Layer 0.
//!
//! Stateless functions and a typed `WorkflowDag` over a `Workflow`'s step list.
//! No engine state, no I/O, no upward calls.

use std::collections::{HashMap, HashSet};

use crate::data::error::DataError;
use crate::data::workflow_definition::WorkflowStep;

/// Validated adjacency representation of a workflow's step graph.
#[derive(Debug, Clone)]
pub struct WorkflowDag {
    /// Insertion order of step names (matches the source workflow).
    order: Vec<String>,
    /// `name -> depends_on names`.
    edges: HashMap<String, Vec<String>>,
}

impl WorkflowDag {
    /// Build and validate a DAG from a slice of steps.
    pub fn build(steps: &[WorkflowStep]) -> Result<Self, DataError> {
        validate_references(steps)?;
        detect_cycle(steps)?;
        let order = steps.iter().map(|s| s.name.clone()).collect();
        let edges = steps
            .iter()
            .map(|s| (s.name.clone(), s.depends_on.clone()))
            .collect();
        Ok(Self { order, edges })
    }

    /// Step names whose dependencies are all in `completed`.
    pub fn ready_steps(&self, completed: &HashSet<String>) -> Vec<String> {
        self.order
            .iter()
            .filter(|name| {
                if completed.contains(*name) {
                    return false;
                }
                self.edges
                    .get(*name)
                    .map(|deps| deps.iter().all(|d| completed.contains(d)))
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    /// Topological order (deps appear before dependents). Stable.
    pub fn topological_order(&self) -> Vec<String> {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for name in &self.order {
            if !visited.contains(name.as_str()) {
                topo_dfs(name, &self.edges, &mut visited, &mut out);
            }
        }
        out
    }

    /// All step names in source order.
    pub fn step_names(&self) -> &[String] {
        &self.order
    }
}

/// Referential integrity check — every `depends_on` must name a real step.
pub fn validate_references(steps: &[WorkflowStep]) -> Result<(), DataError> {
    let names: HashSet<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    for step in steps {
        for dep in &step.depends_on {
            if !names.contains(dep.as_str()) {
                return Err(DataError::MissingDependency {
                    step: step.name.clone(),
                    missing: dep.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Cycle detection using DFS. Returns an error naming a cycle when found.
pub fn detect_cycle(steps: &[WorkflowStep]) -> Result<(), DataError> {
    let adjacency: HashMap<&str, Vec<&str>> = steps
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                s.depends_on.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut in_stack: HashSet<&str> = HashSet::new();
    for step in steps {
        if !visited.contains(step.name.as_str()) {
            cycle_dfs(step.name.as_str(), &adjacency, &mut visited, &mut in_stack)?;
        }
    }
    Ok(())
}

fn cycle_dfs<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    visited: &mut HashSet<&'a str>,
    in_stack: &mut HashSet<&'a str>,
) -> Result<(), DataError> {
    visited.insert(node);
    in_stack.insert(node);
    if let Some(deps) = adj.get(node) {
        for &dep in deps {
            if in_stack.contains(dep) {
                return Err(DataError::CyclicDependency {
                    step: dep.to_string(),
                });
            }
            if !visited.contains(dep) {
                cycle_dfs(dep, adj, visited, in_stack)?;
            }
        }
    }
    in_stack.remove(node);
    Ok(())
}

fn topo_dfs<'a>(
    node: &str,
    edges: &'a HashMap<String, Vec<String>>,
    visited: &mut HashSet<&'a str>,
    out: &mut Vec<String>,
) {
    if visited.contains(node) {
        return;
    }
    if let Some((node_ref, deps)) = edges.get_key_value(node) {
        visited.insert(node_ref.as_str());
        for dep in deps {
            topo_dfs(dep, edges, visited, out);
        }
        out.push(node_ref.clone());
    }
}
