//! Feature lifecycle mechanics, independent of transports and command policy.
pub use crate::data::daemon_state::{FeatureName, FeatureState, FeatureStatus, StopMode};
use crate::engine::{daemon::core::Core, error::EngineError};
pub type FeatureTask = tokio::task::JoinHandle<Result<(), EngineError>>;
#[async_trait::async_trait]
pub trait DaemonFeature: Send + Sync + 'static {
    fn name(&self) -> FeatureName;
    /// Fallible preparation before a configuration transaction commits.
    async fn prepare_start(&self, _core: &Core) -> Result<(), EngineError> {
        Ok(())
    }
    async fn start(&self, core: &Core) -> Result<FeatureTask, EngineError>;
    async fn drain(&self);
    async fn stop_now(&self);
    fn status(&self) -> FeatureStatus;
    fn share(&self) -> usize;
}
