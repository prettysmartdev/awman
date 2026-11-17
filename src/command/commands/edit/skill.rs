//! `edit skill <ref>`: the shared artifact runner for skills.
use super::*;
use crate::data::fs::edit_target::ArtifactKind;

pub(super) async fn run(
    command: &EditCommand,
    f: EditArtifactFlags,
    frontend: &mut dyn EditCommandFrontend,
) -> Result<EditOutcome, CommandError> {
    artifact::run(command, ArtifactKind::Skill, f, frontend).await
}
