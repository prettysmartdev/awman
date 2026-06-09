//! Cross-paradigm exec-into-running-agent abstractions.
//!
//! `AgentExec` is implemented by `BackgroundContainer` (container tier) and,
//! once WI 0090 lands, by the sandbox tier's exec wrapper. It also enables
//! mock testing of `WorkflowEngine::run_setup` / `run_teardown` without a
//! live runtime.

use std::collections::HashMap;

use crate::engine::error::EngineError;

/// Output captured from a single `exec` call into a running agent.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Abstraction over exec-into-running-agent. Both paradigms support exec.
pub trait AgentExec: Send + Sync {
    fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError>;

    /// Execute a command, streaming each output line to `on_line` as it arrives.
    /// The default falls back to `exec` and iterates the buffered output.
    fn exec_streaming(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        let output = self.exec(command, env)?;
        for line in output.stdout.lines() {
            on_line(line);
        }
        for line in output.stderr.lines() {
            on_line(line);
        }
        Ok(output)
    }
}
