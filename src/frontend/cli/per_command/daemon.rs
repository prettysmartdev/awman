//! Presentation of daemon resource responses.
use crate::command::commands::daemon::api::DaemonApiOutcome;
pub fn render(outcome: &DaemonApiOutcome, json: bool) -> String {
    match outcome {
        DaemonApiOutcome::List(text) => text.clone(),
        DaemonApiOutcome::Response(body) if json => serde_json::to_string(body).expect("JSON body"),
        DaemonApiOutcome::Response(serde_json::Value::Object(fields)) => fields
            .iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect::<Vec<_>>()
            .join("\n"),
        DaemonApiOutcome::Response(body) => serde_json::to_string_pretty(body).expect("JSON body"),
    }
}
