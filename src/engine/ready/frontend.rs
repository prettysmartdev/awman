//! `ReadyFrontend` trait — defined by Layer 1, implemented by Layer 3.

use crate::data::ready_phase::ReadyPhase;
use crate::data::ready_summary::ReadySummary;
use crate::engine::agent::AgentImageFrontend;
use crate::engine::error::EngineError;
use crate::engine::ready::host_agent::LocalAgentPingResult;

/// `report_step_status` and `container_frontend` come from
/// [`AgentImageFrontend`]: the ready flow reports image-setup steps through
/// the same two methods the agent engine uses, so they are declared once.
pub trait ReadyFrontend: AgentImageFrontend {
    /// Called when the project Dockerfile (default `Dockerfile.dev` or the
    /// path configured via `RepoConfig.dockerfile`) is missing.
    ///
    /// `dockerfile_path` is the resolved absolute path that the engine
    /// expects the user to confirm creating.
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError>;
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError>;

    fn report_phase(&mut self, phase: &ReadyPhase);
    fn report_summary(&mut self, summary: &ReadySummary);

    /// The sanctioned host-side agent ping finished with this result.
    ///
    /// The ready engine used to compose the `> greeting` / `< response`
    /// transcript itself and push it through `write_message` (WI 0114 F-45:
    /// Layer 1 rendering transcript text). It now reports the result and the
    /// frontend decides how to draw it.
    ///
    /// The default writes those two lines, byte-identical to what the engine
    /// wrote before — so no frontend's output changes until it overrides this.
    /// Only the `Ok` result produced transcript lines; the three failure
    /// results were reported through `report_step_status` alone, and still
    /// are, so the default stays silent for them.
    fn report_ping(&mut self, result: &LocalAgentPingResult) {
        if let LocalAgentPingResult::Ok { greeting, response } = result {
            self.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: format!("> {greeting}"),
            });
            self.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: format!("< {response}"),
            });
        }
    }
}
