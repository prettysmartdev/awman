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
/// The `worker` record's push and pull lines (WI 0143).
fn worker_sides(record: &FeatureRecord) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(push) = &record.push {
        lines.push(format!(
            "    push  {} running  {} queued",
            push.running, push.queued
        ));
    }
    if let Some(pull) = &record.pull {
        if pull.servers.is_empty() {
            lines.push("    pull  no work servers (awman link <url>)".into());
        }
        let name_w = pull.servers.iter().map(|s| s.name.len()).max().unwrap_or(0);
        let state_w = pull
            .servers
            .iter()
            .map(|s| s.state.len())
            .max()
            .unwrap_or(0);
        for server in &pull.servers {
            lines.push(format!(
                "    pull  {:<name_w$}  {:<state_w$}  {} running  {} eligible",
                server.name, server.state, server.running, server.queued_eligible
            ));
        }
    }
    lines
}

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
            if record.state != FeatureState::Off {
                out.extend(worker_sides(record));
            }
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
    if status.listeners.network.is_empty() {
        out.push("  network  off".into());
    }
    for (name, network) in &status.listeners.network {
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
            (_, Some(error)) => out.push(format!(
                "  {name}  {}  error  {}: {error}",
                network.transport, network.listen
            )),
            (Some(bound), None) => out.push(format!(
                "  {name}  {}  {bound}  features: {features}  keys: {}",
                network.transport, network.api_keys
            )),
            (None, None) => out.push(format!(
                "  {name}  {}  starting  {}",
                network.transport, network.listen
            )),
        }
        if network.transport == crate::data::config::network::ListenerTransport::LoopbackHttp {
            out.push(format!(
                "    origins: {}  browsers: {}  links: {}",
                network
                    .allowed_origins
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                network.browser_grants,
                if network.browser_links { "on" } else { "off" }
            ));
        }
        if network.api_keys == 0 {
            out.push(
                "  warning: no API keys, so every network request is refused \
                     (awman daemon api keys.create name=…)"
                    .into(),
            );
        }
    }
    out.join("\n")
}
