//! `StatusCommand` — display the status of all running containers.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

#[derive(Debug, Clone)]
pub struct StatusCommandFlags {
    pub watch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ContainerKind {
    Agent,
    Claws,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusOutcome {
    pub containers: Vec<StatusContainerRow>,
    pub watched: bool,
    /// A randomly-selected hint for the user, refreshed each tick.
    pub tip: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusContainerRow {
    pub id: String,
    pub name: String,
    pub image: String,
    pub started_at: String,
    pub kind: ContainerKind,
    pub tab_number: Option<u32>,
    pub stuck: bool,
    pub command_label: Option<String>,
    /// Live CPU percent reported by the runtime. `None` when stats lookup
    /// failed for this container (transient errors are not fatal — the row
    /// still renders).
    pub cpu_percent: Option<f64>,
    /// Live memory usage in MB.
    pub memory_mb: Option<f64>,
}

fn classify_container(name: &str) -> ContainerKind {
    if name.starts_with("amux-claws-") || name.contains("nanoclaw") {
        ContainerKind::Claws
    } else {
        ContainerKind::Agent
    }
}

/// Optional context supplied by the TUI; CLI / headless leave this `None`.
#[derive(Debug, Clone, Default)]
pub struct StatusCommandTuiContext {
    pub tabs: Vec<TuiTabSnapshot>,
}

#[derive(Debug, Clone)]
pub struct TuiTabSnapshot {
    pub tab_number: u32,
    pub container_name: Option<String>,
    pub is_stuck: bool,
    pub command_label: String,
}

pub trait StatusCommandFrontend: UserMessageSink + Send + Sync {
    /// Optional TUI context. Defaults to `None` for CLI / headless.
    fn tui_context(&self) -> Option<&StatusCommandTuiContext> {
        None
    }

    /// Whether the watch loop should continue. Default: stops after one tick.
    fn should_continue_watching(&mut self) -> bool {
        false
    }

    /// Emit a clear-screen marker so the CLI can redraw the status table in
    /// place. No-op for TUI / headless frontends.
    fn write_clear_marker(&mut self) {}
}

pub struct StatusCommand {
    flags: StatusCommandFlags,
    engines: Engines,
}

impl StatusCommand {
    pub fn new(flags: StatusCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &StatusCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for StatusCommand {
    type Frontend = Box<dyn StatusCommandFrontend>;
    type Outcome = StatusOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "status: gathering session info…".into(),
        });
        let session = match open_session() {
            Ok(s) => s,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("status: failed to open session: {e}"),
                });
                return Err(e);
            }
        };
        let mut last_containers: Vec<StatusContainerRow>;
        let mut tick: u32 = 0;

        loop {
            let handles = match self
                .engines
                .runtime
                .list_running(&session)
            {
                Ok(h) => h,
                Err(e) => {
                    let err = CommandError::from(e);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("status: failed to list running containers: {err}"),
                    });
                    return Err(err);
                }
            };
            let context = frontend.tui_context().cloned();
            let containers: Vec<StatusContainerRow> = handles
                .into_iter()
                .map(|h| {
                    // Best-effort live stats; transient runtime errors are
                    // recorded as `None` rather than failing the row.
                    let stats = self.engines.runtime.stats(&h).ok();
                    let mut row = StatusContainerRow {
                        id: h.id.clone(),
                        name: h.name.clone(),
                        image: h.image_tag.clone(),
                        started_at: h.started_at.to_rfc3339(),
                        kind: classify_container(&h.name),
                        tab_number: None,
                        stuck: false,
                        command_label: None,
                        cpu_percent: stats.as_ref().map(|s| s.cpu_percent),
                        memory_mb: stats.as_ref().map(|s| s.memory_mb),
                    };
                    if let Some(ctx) = context.as_ref() {
                        if let Some(t) = ctx
                            .tabs
                            .iter()
                            .find(|t| t.container_name.as_deref() == Some(&h.name))
                        {
                            row.tab_number = Some(t.tab_number);
                            row.stuck = t.is_stuck;
                            row.command_label = Some(t.command_label.clone());
                        }
                    }
                    row
                })
                .collect();

            // Only emit the clear-screen marker on watch ticks 2+ (the first
            // paint should not blow away whatever the user had above).
            if self.flags.watch && tick > 0 {
                frontend.write_clear_marker();
            }
            tick = tick.saturating_add(1);
            last_containers = containers;

            if !self.flags.watch || !frontend.should_continue_watching() {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("status: found {} running container(s)", last_containers.len()),
        });
        frontend.replay_queued();
        Ok(StatusOutcome {
            containers: last_containers,
            watched: self.flags.watch,
            tip: crate::command::commands::status_tips::select_random_tip().to_string(),
        })
    }
}

impl StatusCommandTuiContext {
    pub fn new(tabs: Vec<TuiTabSnapshot>) -> Self {
        Self { tabs }
    }
}

fn open_session() -> Result<crate::data::session::Session, CommandError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CommandError::Other(format!("cwd unavailable: {e}")))?;
    let resolver = crate::data::session::StaticGitRootResolver::new(cwd.clone());
    crate::data::session::Session::open(
        cwd,
        &resolver,
        crate::data::session::SessionOpenOptions::default(),
    )
    .map_err(CommandError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_context_new_stores_tabs() {
        let tabs = vec![
            TuiTabSnapshot {
                tab_number: 1,
                container_name: Some("amux-abc".into()),
                is_stuck: false,
                command_label: "chat".into(),
            },
            TuiTabSnapshot {
                tab_number: 2,
                container_name: None,
                is_stuck: true,
                command_label: "implement".into(),
            },
        ];
        let ctx = StatusCommandTuiContext::new(tabs.clone());
        assert_eq!(ctx.tabs.len(), 2);
        assert_eq!(ctx.tabs[0].tab_number, 1);
        assert_eq!(ctx.tabs[1].tab_number, 2);
    }

    #[test]
    fn tui_context_enriches_row_with_matching_tab() {
        // Simulate the enrichment logic from run_with_frontend by building
        // a row and applying the TUI context logic directly.
        let ctx = StatusCommandTuiContext::new(vec![
            TuiTabSnapshot {
                tab_number: 3,
                container_name: Some("amux-mycontainer".into()),
                is_stuck: true,
                command_label: "implement 0042".into(),
            },
        ]);
        let name = "amux-mycontainer".to_string();
        let mut row = StatusContainerRow {
            id: "deadbeef1234".into(),
            name: name.clone(),
            image: "amux/dev:latest".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            tab_number: None,
            stuck: false,
            kind: ContainerKind::Agent,
            command_label: None, cpu_percent: None, memory_mb: None,
        };
        // Apply the same matching logic used in run_with_frontend.
        if let Some(t) = ctx.tabs.iter().find(|t| t.container_name.as_deref() == Some(&row.name)) {
            row.tab_number = Some(t.tab_number);
            row.stuck = t.is_stuck;
            row.command_label = Some(t.command_label.clone());
        }
        assert_eq!(row.tab_number, Some(3));
        assert!(row.stuck);
        assert_eq!(row.command_label.as_deref(), Some("implement 0042"));
    }

    #[test]
    fn no_tui_context_leaves_row_fields_none() {
        // When no TUI context is supplied, tab_number, stuck, and
        // command_label stay at their default values.
        let row = StatusContainerRow {
            id: "abc".into(),
            name: "amux-x".into(),
            image: "img".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            kind: ContainerKind::Agent,
            tab_number: None,
            stuck: false,
            command_label: None, cpu_percent: None, memory_mb: None,
        };
        assert_eq!(row.tab_number, None);
        assert!(!row.stuck);
        assert_eq!(row.command_label, None);
    }

    #[test]
    fn tui_context_no_match_leaves_row_unchanged() {
        let ctx = StatusCommandTuiContext::new(vec![
            TuiTabSnapshot {
                tab_number: 1,
                container_name: Some("amux-other".into()),
                is_stuck: false,
                command_label: "chat".into(),
            },
        ]);
        let mut row = StatusContainerRow {
            id: "abc".into(),
            name: "amux-mine".into(),
            image: "img".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            kind: ContainerKind::Agent,
            tab_number: None,
            stuck: false,
            command_label: None, cpu_percent: None, memory_mb: None,
        };
        // The name doesn't match → row stays unchanged.
        if let Some(t) = ctx.tabs.iter().find(|t| t.container_name.as_deref() == Some(&row.name)) {
            row.tab_number = Some(t.tab_number);
            row.stuck = t.is_stuck;
            row.command_label = Some(t.command_label.clone());
        }
        assert_eq!(row.tab_number, None, "no match must leave tab_number None");
    }

    #[test]
    fn classify_agent_containers() {
        assert_eq!(classify_container("amux-123-456"), ContainerKind::Agent);
        assert_eq!(classify_container("amux-abc"), ContainerKind::Agent);
    }

    #[test]
    fn classify_claws_containers() {
        assert_eq!(classify_container("amux-claws-controller"), ContainerKind::Claws);
        assert_eq!(classify_container("amux-claws-abc123"), ContainerKind::Claws);
        assert_eq!(classify_container("nanoclaw-worker-1"), ContainerKind::Claws);
        assert_eq!(classify_container("something-nanoclaw-x"), ContainerKind::Claws);
    }
}
