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

fn state_label(record: &FeatureRecord) -> &'static str {
    match record.state {
        FeatureState::Off => "off",
        FeatureState::On => "on",
        FeatureState::Draining => "draining",
        FeatureState::Degraded => "degraded",
        FeatureState::Ephemeral => "ephemeral",
    }
}

/// The `worker` record's push and pull lines (WI 0143).
fn worker_sides(record: &FeatureRecord) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(push) = &record.push {
        lines.push(format!(
            "    Push: {} running, {} queued",
            push.running, push.queued
        ));
    }
    if let Some(pull) = &record.pull {
        if pull.servers.is_empty() {
            lines.push("    Pull: no work servers (awman link <url>)".into());
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
                "    Pull: {:<name_w$}  {:<state_w$}  {} running, {} eligible",
                server.name, server.state, server.running, server.queued_eligible
            ));
        }
    }
    lines
}

/// The `awman daemon status` text form.
pub fn render_status(status: &StatusResponse) -> String {
    let running: usize = status.features.values().map(|f| f.running).sum();
    let mut out = vec![
        "awman daemon is running".into(),
        format!("  PID       {}", status.pid),
        format!("  Uptime    {}", duration(status.uptime_ms)),
        format!("  Version   {}", status.version),
        format!(
            "  Capacity  {running}/{} running",
            status.max_concurrent_jobs
        ),
        String::new(),
        "Features".into(),
    ];
    let enabled = status
        .features
        .values()
        .any(|f| f.state != FeatureState::Off);
    if !enabled {
        out.push("  None enabled (the daemon stays up; awman daemon stop ends it)".into());
    } else {
        let rows: Vec<_> = status
            .features
            .iter()
            .map(|(name, record)| {
                let (running, queued) = if record.state == FeatureState::Off {
                    ("-".into(), "-".into())
                } else {
                    (
                        record
                            .max_concurrent_jobs
                            .map(|max| format!("{}/{max}", record.running))
                            .unwrap_or_else(|| record.running.to_string()),
                        record.queued.to_string(),
                    )
                };
                (name.as_str(), state_label(record), running, queued, record)
            })
            .collect();
        let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
        let state_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(5);
        let running_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(7);
        let queued_w = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(6);
        out.push(format!(
            "  {:<name_w$}  {:<state_w$}  {:>running_w$}  {:>queued_w$}",
            "NAME", "STATE", "RUNNING", "QUEUED"
        ));
        for (name, label, running, queued, record) in rows {
            out.push(format!(
                "  {name:<name_w$}  {label:<state_w$}  {running:>running_w$}  {queued:>queued_w$}"
            ));
            if name == "engine" {
                let instances = record
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.get("instances"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                out.push(format!("    Instances: {instances}"));
            }
            if record.state != FeatureState::Off {
                if record.queued > 0 {
                    if let Some(age) = record.oldest_queued_age_ms {
                        out.push(format!("    Oldest queued: {}", duration(age)));
                    }
                }
                if record.state == FeatureState::Ephemeral {
                    if let Some(reason) = &record.reason {
                        out.push(format!("    Reason: {reason}"));
                    }
                }
                if let (Some(timeout), Some(idle)) = (record.idle_timeout_ms, record.idle_ms) {
                    if record.state == FeatureState::Ephemeral {
                        out.push(format!(
                            "    Idles out in {}",
                            duration(timeout.saturating_sub(idle))
                        ));
                    }
                }
            }
            if record.state == FeatureState::Degraded {
                out.push(format!("    Restarts: {}", record.restarts));
                if let Some(error) = &record.last_error {
                    out.push(format!("    Last error: {error}"));
                }
            }
            if record.state != FeatureState::Off {
                out.extend(worker_sides(record));
            }
        }
    }
    out.push(String::new());
    out.push("Listeners".into());
    match &status.listeners.local {
        Some(local) => out.push(format!(
            "  local  {}\n    Address: {}",
            match local.transport {
                LocalTransport::UnixSocket => "unix socket",
                LocalTransport::NamedPipe => "named pipe",
            },
            local.address
        )),
        None => out.push("  local  not bound".into()),
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
                .join(", ")
        };
        out.push(format!("  {name}  {}", network.transport));
        match (&network.bound, &network.error) {
            (_, Some(error)) => out.push(format!(
                "    Listen: {}\n    Error: {error}",
                network.listen
            )),
            (Some(bound), None) => out.push(format!("    Address: {bound}")),
            (None, None) => out.push(format!(
                "    Listen: {}\n    Status: starting",
                network.listen
            )),
        }
        out.push(format!("    Features: {features}"));
        out.push(format!("    API keys: {}", network.api_keys));
        if network.transport == crate::data::config::network::ListenerTransport::LoopbackHttp {
            out.push(format!(
                "    Origins: {}\n    Browsers: {}\n    Browser links: {}",
                if network.allowed_origins.is_empty() {
                    "none".into()
                } else {
                    network
                        .allowed_origins
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                },
                network.browser_grants,
                if network.browser_links { "on" } else { "off" }
            ));
        }
        if network.api_keys == 0 {
            out.push(
                "    Warning: no API keys, so every network request is refused\n    Create a key: awman daemon api keys.create name=…"
                    .into(),
            );
        }
    }
    out.join("\n")
}
