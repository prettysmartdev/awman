//! `edit agent <ref>`: the shared artifact runner for agents.
use super::*;
use crate::data::fs::edit_target::ArtifactKind;

pub(super) async fn run(
    command: &EditCommand,
    f: EditArtifactFlags,
    frontend: &mut dyn EditCommandFrontend,
) -> Result<EditOutcome, CommandError> {
    artifact::run(command, ArtifactKind::Agent, f, frontend).await
}
