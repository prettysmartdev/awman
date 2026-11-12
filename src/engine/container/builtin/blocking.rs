//! Synchronous runtime operations stay callable from the synchronous instance
//! API. On a Tokio worker, temporarily hand that worker back to the scheduler
//! and execute the operation in its blocking pool. Synchronous calls entered
//! under a current-thread runtime are refused before they wait. Builtin
//! launches require a multithread Tokio runtime; pure synchronous driver/state
//! operations also remain callable on threads outside Tokio.
use crate::engine::error::EngineError;

pub(super) struct BlockingOperation;
impl BlockingOperation {
    pub fn check_context() -> Result<(), EngineError> {
        if tokio::runtime::Handle::try_current().is_ok_and(|runtime| {
            runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread
        }) {
            return Err(EngineError::BuiltinBlockingContextUnsupported);
        }
        Ok(())
    }

    /// Borrowed streaming callbacks cannot move to the blocking pool. On a
    /// multithread worker, block_in_place releases that worker's scheduler.
    pub fn run_borrowed<T>(
        operation: impl FnOnce() -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        Self::check_context()?;
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(operation)
        } else {
            operation()
        }
    }

    pub fn run<T: Send + 'static>(
        operation: impl FnOnce() -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        Self::check_context()?;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime)
                if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
            {
                tokio::task::block_in_place(|| {
                    runtime.block_on(async {
                        tokio::task::spawn_blocking(operation)
                            .await
                            // A panic payload may contain SDK config or frontend data.
                            .map_err(|_| {
                                EngineError::Other("builtin blocking operation failed".into())
                            })?
                    })
                })
            }
            Ok(_) | Err(_) => operation(),
        }
    }
}
