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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn operation_runs_on_the_blocking_pool_and_worker_keeps_servicing_tasks() {
        let (release, received) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            release.send(()).unwrap();
        });
        BlockingOperation::run(move || {
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            Ok(())
        })
        .unwrap();
    }
    #[tokio::test]
    async fn current_thread_runtime_can_call_from_a_thread_outside_tokio() {
        assert_eq!(
            std::thread::spawn(|| BlockingOperation::run(|| Ok(7)))
                .join()
                .unwrap()
                .unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn current_thread_runtime_can_call_directly_without_panicking() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inside = ran.clone();
        let heartbeat = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            17
        });
        let result = BlockingOperation::run(move || {
            inside.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(11)
        });
        assert!(
            result.is_err(),
            "direct current-thread calls must refuse before blocking"
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(heartbeat.await.unwrap(), 17);
    }
}
