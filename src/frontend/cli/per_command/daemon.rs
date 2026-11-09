//! `awman daemon …` for the CLI: hosting the daemon process and rendering
//! lifecycle, status and resource responses.
use async_trait::async_trait;

use crate::command::commands::daemon::api::DaemonApiOutcome;
use crate::command::commands::daemon::lifecycle::{
    DaemonCommandFrontend, DaemonOutcome, DaemonStatusOutcome,
};
use crate::command::daemon_api::core::{FeatureRecord, StatusResponse};
use crate::command::daemon_runtime::LocalTransport;
use crate::command::error::CommandError;
use crate::data::daemon_state::FeatureState;
use crate::frontend::cli::command_frontend::CliFrontend;

#[async_trait]
impl DaemonCommandFrontend for CliFrontend {
    async fn host_daemon(
        &mut self,
        env: crate::data::config::env::EnvSnapshot,
        set: crate::data::config::overrides::ConfigOverrides,
    ) -> Result<(), CommandError> {
        eprintln!("awman daemon starting (Ctrl-C stops it)");
        crate::frontend::daemon::run(env, set).await
    }

    /// Log lines are the command's output: stdout, unbuffered by the queue.
    fn log_line(&mut self, line: &str) {
        println!("{line}");
    }
}

pub fn render_api(outcome: &DaemonApiOutcome, json: bool) -> String {
    match outcome {
        DaemonApiOutcome::List(text) => text.trim_end().to_string(),
        DaemonApiOutcome::Response(body) if json => serde_json::to_string(body).expect("JSON body"),
        DaemonApiOutcome::Response(serde_json::Value::Object(fields)) => {
            let mut text = fields
                .iter()
                .map(|(key, value)| match value {
                    serde_json::Value::String(s) => format!("{key}: {s}"),
                    other => format!("{key}: {other}"),
                })
                .collect::<Vec<_>>()
                .join("\n");
            if fields.contains_key("key")
                && fields
                    .get("id")
                    .and_then(|id| id.as_str())
                    .is_some_and(|id| id.starts_with("key_"))
            {
                text.push_str("\n\nStore this key now: it is not shown again.");
            }
            text
        }
        DaemonApiOutcome::Response(serde_json::Value::Null) => String::new(),
        DaemonApiOutcome::Response(body) => serde_json::to_string_pretty(body).expect("JSON body"),
    }
}

pub fn render(outcome: &DaemonOutcome, json: bool) -> Option<String> {
    if json {
        return Some(match outcome {
            DaemonOutcome::Status(status) => {
                serde_json::to_string_pretty(status).expect("status serializes")
            }
            other => serde_json::to_string(other).expect("outcome serializes"),
        });
    }
    match outcome {
        // The foreground daemon has already run to completion.
        DaemonOutcome::Start(start) if !start.background => None,
        DaemonOutcome::Start(start) if start.ready => Some(format!(
            "awman daemon started in the background (PID {}). Logs: {}",
            start.pid, start.log_file
        )),
        DaemonOutcome::Start(start) => Some(format!(
            "awman daemon launched (PID {}) but is not answering yet; check `awman daemon logs` ({})",
            start.pid, start.log_file
        )),
        DaemonOutcome::Stop(stop) => {
            let pid = stop.stopped_pid.unwrap_or_default();
            let mut text = if stop.exited {
                format!("awman daemon (PID {pid}) stopped.")
            } else {
                format!("awman daemon (PID {pid}) is stopping; running work is still draining.")
            };
            if !stop.canceled.is_empty() {
                text.push_str(&format!("\ncancelled {} job(s)", stop.canceled.len()));
            }
            Some(text)
        }
        DaemonOutcome::Status(status) => Some(render_status_outcome(status)),
        DaemonOutcome::Logs(logs) if !logs.found => {
            Some(format!("No daemon log yet at {}", logs.log_file))
        }
        DaemonOutcome::Logs(_) => None,
    }
}

fn render_status_outcome(outcome: &DaemonStatusOutcome) -> String {
    if outcome.json {
        return serde_json::to_string_pretty(outcome).expect("status serializes");
    }
    match &outcome.status {
        Some(status) if outcome.running => render_status(status),
        _ => "awman daemon is not running".into(),
    }
}

/// Format milliseconds as `4.1s`, `2m3s`, `1h2m`.
pub fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        if secs < 10 && !ms.is_multiple_of(1000) {
            return format!("{:.1}s", ms as f64 / 1000.0);
        }
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m{}s", secs / 60, secs % 60);
    }
    format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
}

fn state_label(record: &FeatureRecord) -> String {
    match record.state {
        FeatureState::Off => "off".into(),
        FeatureState::On => "on".into(),
        FeatureState::Draining => "draining".into(),
        FeatureState::Degraded => "degraded".into(),
        FeatureState::Ephemeral => match &record.reason {
            Some(reason) => format!("ephemeral ({reason})"),
            None => "ephemeral".into(),
        },
    }
}

/// The `awman daemon status` text form.
pub fn render_status(status: &StatusResponse) -> String {
    let mut out = vec![format!(
        "awman daemon  pid {}  up {}  version {}",
        status.pid,
        duration(status.uptime_ms),
        status.version
    )];
    let running: usize = status.features.values().map(|f| f.running).sum();
    out.push(format!(
        "capacity  {running}/{} running",
        status.max_concurrent_jobs
    ));
    if let Some(engine) = status.features.get("engine") {
        let instances = engine
            .detail
            .as_ref()
            .and_then(|detail| detail.get("instances"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        out.push(format!(
            "engine  {}  {instances} instances",
            state_label(engine)
        ));
    }
    let enabled = status
        .features
        .values()
        .any(|f| f.state != FeatureState::Off);
    if !enabled {
        out.push("features: none enabled (the daemon stays up; awman daemon stop ends it)".into());
    } else {
        out.push("features".into());
        let labels: Vec<(String, String)> = status
            .features
            .iter()
            .map(|(name, record)| (name.clone(), state_label(record)))
            .collect();
        let name_w = labels.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        let state_w = labels.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        for ((name, label), record) in labels.iter().zip(status.features.values()) {
            let mut line = format!("  {name:<name_w$}  {label:<state_w$}");
            if record.state != FeatureState::Off {
                let share = record
                    .max_concurrent_jobs
                    .map(|max| format!("{}/{max}", record.running))
                    .unwrap_or_else(|| record.running.to_string());
                line.push_str(&format!("  {share} running"));
                if record.queued > 0 {
                    line.push_str(&format!("  {} queued", record.queued));
                    if let Some(age) = record.oldest_queued_age_ms {
                        line.push_str(&format!(" (oldest {})", duration(age)));
                    }
                }
                if let (Some(timeout), Some(idle)) = (record.idle_timeout_ms, record.idle_ms) {
                    if record.state == FeatureState::Ephemeral {
                        line.push_str(&format!(
                            "  idles out in {}",
                            duration(timeout.saturating_sub(idle))
                        ));
                    }
                }
            }
            if record.state == FeatureState::Degraded {
                line.push_str(&format!("  restarts {}", record.restarts));
                if let Some(error) = &record.last_error {
                    line.push_str(&format!("  last error: {error}"));
                }
            }
            out.push(line.trim_end().to_string());
        }
    }
    out.push("listeners".into());
    match &status.listeners.local {
        Some(local) => out.push(format!(
            "  local    {}  {}",
            match local.transport {
                LocalTransport::UnixSocket => "unix socket",
                LocalTransport::NamedPipe => "named pipe",
            },
            local.address
        )),
        None => out.push("  local    not bound".into()),
    }
    match &status.listeners.network {
        None => out.push("  network  off".into()),
        Some(network) => {
            let features = if network.features.is_empty() {
                "none".to_string()
            } else {
                network
                    .features
                    .iter()
                    .map(|f| f.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            match (&network.bound, &network.error) {
                (_, Some(error)) => {
                    out.push(format!("  network  error  {}: {error}", network.listen))
                }
                (Some(bound), None) => out.push(format!(
                    "  network  tls  {bound}  features: {features}  keys: {}",
                    network.api_keys
                )),
                (None, None) => out.push(format!("  network  starting  {}", network.listen)),
            }
            if network.api_keys == 0 {
                out.push(
                    "  warning: no API keys, so every network request is refused \
                     (awman daemon api keys.create name=…)"
                        .into(),
                );
            }
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::daemon_runtime::network::{
        ListenerStatus, LocalListenerRecord, NetworkListenerRecord,
    };
    use crate::data::daemon_state::{EnabledBy, FeatureName};

    fn record(name: &str, state: FeatureState, running: usize, max: usize) -> FeatureRecord {
        FeatureRecord {
            enabled_by: (state != FeatureState::Off).then_some(EnabledBy::Config),
            reason: None,
            oldest_queued_age_ms: None,
            restarts: 0,
            last_error: None,
            since_at: chrono::Utc::now(),
            idle_timeout_ms: None,
            idle_ms: None,
            name: name.into(),
            state,
            max_concurrent_jobs: Some(max),
            running,
            queued: 0,
            detail: None,
        }
    }

    fn status(
        features: Vec<FeatureRecord>,
        network: Option<NetworkListenerRecord>,
    ) -> StatusResponse {
        StatusResponse {
            status: "ok".into(),
            version: "0.9.0".into(),
            protocol: [("api".to_string(), 1)].into(),
            capabilities: vec![],
            features: features.into_iter().map(|f| (f.name.clone(), f)).collect(),
            max_concurrent_jobs: 4,
            pid: 4242,
            uptime_ms: 123_000,
            listeners: ListenerStatus {
                local: Some(LocalListenerRecord {
                    transport: LocalTransport::UnixSocket,
                    address: "/h/.awman/data/daemon/api.sock".into(),
                }),
                network,
            },
            agents: vec![],
        }
    }

    fn network(error: Option<&str>, keys: usize) -> NetworkListenerRecord {
        NetworkListenerRecord {
            enabled: true,
            listen: "0.0.0.0:9876".into(),
            bound: error.is_none().then(|| "0.0.0.0:9876".parse().unwrap()),
            tls: None,
            error: error.map(str::to_string),
            features: vec![FeatureName::Worker],
            api_keys: keys,
        }
    }

    #[test]
    fn durations() {
        assert_eq!(duration(4100), "4.1s");
        assert_eq!(duration(12_000), "12s");
        assert_eq!(duration(123_000), "2m3s");
        assert_eq!(duration(888_000), "14m48s");
        assert_eq!(duration(3_720_000), "1h2m");
    }

    #[test]
    fn status_shows_every_feature_state_running_queued_and_shares() {
        let mut squad = record("squad", FeatureState::On, 1, 1);
        squad.queued = 2;
        squad.oldest_queued_age_ms = Some(4100);
        let worker = record("worker", FeatureState::Draining, 1, 2);
        let mut engine = record("engine", FeatureState::Ephemeral, 0, 4);
        engine.reason = Some("rex".into());
        engine.detail = Some(serde_json::json!({"instances": 3, "frontends": 4}));
        engine.idle_timeout_ms = Some(900_000);
        engine.idle_ms = Some(12_000);
        let text = render_status(&status(vec![squad, worker, engine], Some(network(None, 2))));
        assert!(
            text.starts_with("awman daemon  pid 4242  up 2m3s"),
            "{text}"
        );
        assert!(text.contains("capacity  2/4 running"), "{text}");
        assert!(text.contains("squad   on"), "{text}");
        assert!(
            text.contains("1/1 running  2 queued (oldest 4.1s)"),
            "{text}"
        );
        assert!(text.contains("worker  draining"), "{text}");
        assert!(text.contains("1/2 running"), "{text}");
        assert!(
            text.contains("engine  ephemeral (rex)  3 instances"),
            "{text}"
        );
        assert!(text.contains("0/4 running  idles out in 14m48s"), "{text}");
        assert!(
            text.contains("local    unix socket  /h/.awman/data/daemon/api.sock"),
            "{text}"
        );
        assert!(
            text.contains("network  tls  0.0.0.0:9876  features: worker  keys: 2"),
            "{text}"
        );
        assert!(!text.contains("warning"), "{text}");
    }

    #[test]
    fn status_shows_off_degraded_no_features_bind_errors_and_no_keys() {
        let mut squad = record("squad", FeatureState::Degraded, 0, 1);
        squad.restarts = 3;
        squad.last_error = Some("boom".into());
        let text = render_status(&status(
            vec![squad, record("worker", FeatureState::Off, 0, 2)],
            Some(network(Some("address in use"), 0)),
        ));
        assert!(text.contains("degraded"), "{text}");
        assert!(text.contains("restarts 3  last error: boom"), "{text}");
        assert!(text.contains("worker  off"), "{text}");
        assert!(
            text.contains("network  error  0.0.0.0:9876: address in use"),
            "{text}"
        );
        assert!(text.contains("warning: no API keys"), "{text}");

        let none = render_status(&status(
            vec![record("worker", FeatureState::Off, 0, 2)],
            None,
        ));
        assert!(
            none.contains(
                "features: none enabled (the daemon stays up; awman daemon stop ends it)"
            ),
            "{none}"
        );
        assert!(none.contains("network  off"), "{none}");
    }

    #[test]
    fn engine_status_shows_off_and_zero_instances_when_disabled() {
        let text = render_status(&status(
            vec![record("engine", FeatureState::Off, 0, 4)],
            None,
        ));
        assert!(text.contains("engine  off  0 instances"), "{text}");
    }

    #[test]
    fn not_running_text_and_json() {
        let outcome = DaemonStatusOutcome {
            running: false,
            status: None,
            json: false,
        };
        assert_eq!(
            render_status_outcome(&outcome),
            "awman daemon is not running"
        );
        let json = DaemonStatusOutcome {
            json: true,
            ..outcome
        };
        let value: serde_json::Value = serde_json::from_str(&render_status_outcome(&json)).unwrap();
        assert_eq!(value, serde_json::json!({"running": false}));
    }

    #[test]
    fn a_created_key_is_flagged_as_shown_once() {
        let text = render_api(
            &DaemonApiOutcome::Response(serde_json::json!({
                "id": crate::data::ids::KeyId::new().to_string(),
                "name": "ci",
                "key": "secret",
                "created_at": "2026-11-06T00:00:00Z"
            })),
            false,
        );
        assert!(text.contains("key: secret"), "{text}");
        assert!(text.contains("not shown again"), "{text}");
    }

    #[test]
    fn api_json_output_preserves_the_route_response_shape() {
        let response = serde_json::json!({
            "items": [{"id": "ins_00000000000000000000000000000000", "annotations": {"repo": "acme"}}],
            "next_cursor": null
        });
        let output = render_api(&DaemonApiOutcome::Response(response.clone()), true);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap(),
            response
        );
    }
}
