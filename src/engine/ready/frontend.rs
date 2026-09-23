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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::{MessageLevel, RecordingMessageSink, UserMessage, UserMessageSink};
    use crate::data::setup_step::SetupStep;
    use crate::data::step_status::StepStatus;

    /// A frontend that implements nothing beyond the required methods, so
    /// `report_ping` runs its default.
    #[derive(Default)]
    struct DefaultOnlyFrontend(RecordingMessageSink);

    impl UserMessageSink for DefaultOnlyFrontend {
        fn write_message(&mut self, message: UserMessage) {
            self.0.write_message(message);
        }
        fn replay_queued(&mut self) {}
    }
    impl AgentImageFrontend for DefaultOnlyFrontend {
        fn report_step_status(&mut self, _step: &SetupStep, _status: StepStatus) {}
        fn container_frontend(
            &mut self,
        ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
            unreachable!("not exercised")
        }
    }
    impl ReadyFrontend for DefaultOnlyFrontend {
        fn ask_create_dockerfile(&mut self, _: &std::path::Path) -> Result<bool, EngineError> {
            Ok(false)
        }
        fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
            Ok(false)
        }
        fn report_phase(&mut self, _phase: &ReadyPhase) {}
        fn report_summary(&mut self, _summary: &ReadySummary) {}
    }

    /// F-45's contract: the default `report_ping` writes the identical two
    /// transcript lines the ready engine used to compose itself, so a
    /// frontend that has not opted in shows exactly the same output.
    #[test]
    fn the_default_report_ping_writes_the_pre_f45_transcript() {
        let mut fe = DefaultOnlyFrontend::default();
        fe.report_ping(&LocalAgentPingResult::Ok {
            greeting: "Hello".into(),
            response: "Hi! How can I help?".into(),
        });

        let messages = fe.0.all();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "> Hello");
        assert_eq!(messages[0].level, MessageLevel::Info);
        assert_eq!(messages[1].text, "< Hi! How can I help?");
        assert_eq!(messages[1].level, MessageLevel::Info);
    }

    /// Only a successful ping produced transcript lines before F-45; the three
    /// failure results were reported through `report_step_status` alone, and
    /// the default must stay silent for them or a failed ready-check would
    /// grow two blank lines it never had.
    #[test]
    fn the_default_report_ping_is_silent_for_every_failure_result() {
        for result in [
            LocalAgentPingResult::Error,
            LocalAgentPingResult::NotInstalled,
            LocalAgentPingResult::CouldNotRun,
        ] {
            let mut fe = DefaultOnlyFrontend::default();
            fe.report_ping(&result);
            assert!(
                fe.0.all().is_empty(),
                "{result:?} must write no transcript line"
            );
        }
    }
}
