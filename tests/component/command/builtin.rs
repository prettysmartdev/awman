use super::scenarios;
use crate::support::{
    self,
    backend::{Backend, BackendKind},
};

#[tokio::test(flavor = "multi_thread")]
async fn catalogue_explains_each_command_before_execution() {
    let s = support::sandbox("command::builtin::catalogue_explains_each_command_before_execution");
    let host_agents = scenarios::prepare(&s);
    let backend = Backend::start(BackendKind::Builtin, &s).await;
    scenarios::catalogue_explains_each_command_before_execution(&backend, &s).await;
    scenarios::assert_no_host_agents(&host_agents);
    backend.shutdown().await;
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn config_edits_are_durable_and_invalid_values_are_atomic() {
    let s = support::sandbox(
        "command::builtin::config_edits_are_durable_and_invalid_values_are_atomic",
    );
    let host_agents = scenarios::prepare(&s);
    let backend = Backend::start(BackendKind::Builtin, &s).await;
    scenarios::config_edits_are_durable_and_invalid_values_are_atomic(&backend, &s).await;
    scenarios::assert_no_host_agents(&host_agents);
    backend.shutdown().await;
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn workflow_preflight_is_read_only_and_missing_work_is_typed() {
    let s = support::sandbox(
        "command::builtin::workflow_preflight_is_read_only_and_missing_work_is_typed",
    );
    let host_agents = scenarios::prepare(&s);
    let backend = Backend::start(BackendKind::Builtin, &s).await;
    scenarios::workflow_preflight_is_read_only_and_missing_work_is_typed(&backend, &s).await;
    scenarios::assert_no_host_agents(&host_agents);
    backend.shutdown().await;
    s.finish();
}
