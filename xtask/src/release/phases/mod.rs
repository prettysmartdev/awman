//! One function per phase of design 3.1; `run.rs` decides which to run.
mod hosted;
mod publish;
pub mod seal;

use super::error::{Step, Stop};
use super::run::Ctx;

pub fn run(ctx: &mut Ctx, phase: &str) -> Step {
    match phase {
        "push" => hosted::push(ctx),
        "dispatch" => hosted::dispatch(ctx),
        "collect" => hosted::collect(ctx),
        "native" => hosted::native_phase(ctx),
        "seal" => seal::seal(ctx),
        "confirm" => seal::confirm(ctx),
        "land" => publish::land(ctx),
        "tag" => publish::tag(ctx),
        "publish" => publish::publish(ctx),
        "verify" => publish::verify(ctx),
        "cleanup" => publish::cleanup(ctx),
        other => Err(Stop::new("state-corrupt", format!("unknown phase {other}"))),
    }
}
