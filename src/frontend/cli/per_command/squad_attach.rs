//! CLI presentation for the Layer 2 `squad attach` command.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::attach::{
    format_candidates, SquadAttachFrontend, SquadContainer,
};
use crate::command::error::CommandError;
use crate::engine::agent_runtime::AgentInstance;
use crate::frontend::cli::CliFrontend;

impl SquadAttachFrontend for CliFrontend {
    fn begin_attach(&mut self, _task: &str) -> Result<CancellationToken, CommandError> {
        let cancel = CancellationToken::new();
        self.squad_attach_cancel = Some(cancel.clone());
        self.squad_attach_exit_code.store(0, Ordering::Relaxed);
        Ok(cancel)
    }

    fn ask_pick_candidate(
        &mut self,
        candidates: &[SquadContainer],
    ) -> Result<Option<usize>, CommandError> {
        if self.non_interactive {
            return Ok(None);
        }
        let items = format_candidates(candidates);
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        let Some(picked) = super::helpers::pick_numbered("attach to which container?", &refs, 1)
        else {
            // stdin ended without a choice; the same answer a headless run gets.
            return Ok(None);
        };
        Ok((1..=candidates.len())
            .contains(&picked)
            .then_some(picked - 1))
    }

    fn on_slot_attached(
        &mut self,
        step: &str,
        instance: Box<dyn AgentInstance>,
    ) -> Result<(), CommandError> {
        let matches = self.matches.clone();
        let mut execution = instance
            .run_with_frontend(Box::new(CliFrontend::new(matches.clone())))
            .map_err(CommandError::from)?;
        let cancel = self.squad_attach_cancel.clone().unwrap_or_default();
        let exit_code: Arc<AtomicI32> = self.squad_attach_exit_code.clone();
        let explicit_target = self
            .matches
            .subcommand_matches("squad")
            .and_then(|squad| squad.subcommand_matches("attach"))
            .and_then(|attach| attach.get_one::<String>("container"))
            .is_some();
        let single_attach = step == "evaluation" || explicit_target;
        tokio::spawn(async move {
            let code = execution
                .wait()
                .await
                .map(|info| info.exit_code)
                .unwrap_or(1);
            if single_attach || code != 0 {
                exit_code.store(code, Ordering::Relaxed);
            }
            if single_attach {
                cancel.cancel();
            }
        });
        Ok(())
    }

    fn on_slot_exited(&mut self, _step: &str) {}

    fn exit_code(&self) -> i32 {
        self.squad_attach_exit_code.load(Ordering::Relaxed)
    }
}
