//! The `Command` trait every `*Command` struct implements.
//!
//! Each command owns its own `Frontend` associated type and its own `Outcome`
//! type. The trait carries no engine references; commands hold those in their
//! struct fields, populated at construction by Dispatch.

use async_trait::async_trait;

use crate::command::error::CommandError;

#[async_trait]
pub trait Command {
    /// The per-command frontend trait this command uses.
    type Frontend: Send;
    /// The typed outcome this command returns on success.
    type Outcome;

    /// Drive the command to completion, routing all I/O through `frontend`.
    async fn run_with_frontend(
        self,
        frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError>;
}
