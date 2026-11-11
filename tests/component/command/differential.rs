use super::scenarios;
use crate::support::{
    self,
    backend::{Backend, BackendKind},
};

#[tokio::test(flavor = "multi_thread")]
async fn command_traces_match_between_builtin_and_daemon() {
    let mut traces = Vec::new();
    for kind in [BackendKind::Builtin, BackendKind::Daemon] {
        let s = support::sandbox(
            "command::differential::command_traces_match_between_builtin_and_daemon",
        );
        let host_agents = scenarios::prepare(&s);
        let backend = Backend::start(kind, &s).await;
        let trace = vec![
            scenarios::catalogue_explains_each_command_before_execution(&backend, &s).await,
            scenarios::config_edits_are_durable_and_invalid_values_are_atomic(&backend, &s).await,
            scenarios::workflow_preflight_is_read_only_and_missing_work_is_typed(&backend, &s)
                .await,
        ];
        traces.push(trace);
        scenarios::assert_no_host_agents(&host_agents);
        backend.shutdown().await;
        s.finish();
    }
    assert_eq!(traces[0], traces[1]);
}
