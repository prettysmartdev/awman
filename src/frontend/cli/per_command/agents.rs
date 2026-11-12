//! CLI presentation for `awman agents`.

use std::io::{IsTerminal, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use tokio_util::sync::CancellationToken;

use crate::command::commands::agents::{
    AgentSnapshot, AgentState, AgentsCommandFrontend, AgentsDetailOutcome, AgentsOutcome,
    AgentsOverview, KillChoice, LIST_COLUMNS, STATUS_COLUMNS,
};
use crate::command::error::CommandError;
use crate::data::prompt::Prompt;
use crate::frontend::cli::command_frontend::CliFrontend;

use crate::frontend::render_helpers::format_table;

static WATCH_SCREEN_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn install_watch_panic_hook() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if WATCH_SCREEN_ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let mut out = std::io::stdout();
                let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
                let _ = out.flush();
            }
            previous(info);
        }));
    });
}

fn watch_signals(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let interrupt = signal(SignalKind::interrupt());
        let terminate = signal(SignalKind::terminate());
        let hangup = signal(SignalKind::hangup());
        tokio::spawn(async move {
            if let (Ok(mut interrupt), Ok(mut terminate), Ok(mut hangup)) =
                (interrupt, terminate, hangup)
            {
                tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {}, _ = hangup.recv() => {} }
                cancel.cancel();
            }
        })
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        })
    }
}

pub(crate) struct WatchScreen {
    writer: Box<dyn Write + Send + Sync>,
    tty: bool,
    frames: usize,
    last: Option<AgentsOverview>,
    signal_task: tokio::task::JoinHandle<()>,
}

impl WatchScreen {
    pub(crate) fn new(
        mut writer: Box<dyn Write + Send + Sync>,
        tty: bool,
        signal_task: tokio::task::JoinHandle<()>,
    ) -> Self {
        if tty {
            let _ = writer.write_all(b"\x1b[?1049h\x1b[?25l");
            let _ = writer.flush();
        }
        Self {
            writer,
            tty,
            frames: 0,
            last: None,
            signal_task,
        }
    }

    fn draw(&mut self, overview: &AgentsOverview) {
        let height = crossterm::terminal::size()
            .map(|(_, height)| height as usize)
            .unwrap_or(24);
        let max_rows = if self.tty {
            Some(height.saturating_sub(9))
        } else {
            None
        };
        let frame = render_overview(overview, terminal_width(self.tty), max_rows);
        if self.tty {
            let _ = self.writer.write_all(b"\x1b[H\x1b[J");
        } else if self.frames > 0 {
            let _ = self.writer.write_all(b"\n");
        }
        let _ = self.writer.write_all(frame.as_bytes());
        let _ = self.writer.flush();
        self.frames += 1;
        self.last = Some(overview.clone());
    }
}

impl Drop for WatchScreen {
    fn drop(&mut self) {
        self.signal_task.abort();
        WATCH_SCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        if self.tty {
            let _ = self.writer.write_all(b"\r\x1b[2K\x1b[?25h\x1b[?1049l");
            let _ = self.writer.flush();
        }
    }
}

impl AgentsCommandFrontend for CliFrontend {
    fn attach_cancel(&self) -> CancellationToken {
        self.agents_attach_cancel.clone()
    }

    fn begin_watch(&mut self) -> CancellationToken {
        let cancel = CancellationToken::new();
        // Register eagerly, before the alternate screen becomes visible.
        let signal_task = watch_signals(cancel.clone());
        install_watch_panic_hook();
        WATCH_SCREEN_ACTIVE.store(
            std::io::stdout().is_terminal(),
            std::sync::atomic::Ordering::SeqCst,
        );
        self.agents_watch = Some(WatchScreen::new(
            Box::new(std::io::stdout()),
            std::io::stdout().is_terminal(),
            signal_task,
        ));
        cancel
    }

    fn render_overview(&mut self, overview: &AgentsOverview, watching: bool) {
        if watching {
            if let Some(screen) = self.agents_watch.as_mut() {
                screen.draw(overview);
            }
        }
    }

    fn end_watch(&mut self) {
        if let Some(screen) = self.agents_watch.take() {
            let final_snapshot = if screen.tty {
                screen.last.clone()
            } else {
                None
            };
            drop(screen);
            if let Some(overview) = final_snapshot {
                print!("{}", render_overview(&overview, terminal_width(true), None));
            }
        }
    }

    fn confirm_kill(&mut self, prompt: &Prompt<KillChoice>) -> Result<KillChoice, CommandError> {
        if self.non_interactive || self.is_json_mode() {
            return Err(CommandError::InteractiveInputUnavailable {
                prompt: "yes".into(),
            });
        }
        self.pick_from_prompt(prompt)
    }
}

pub(crate) fn render_agents(outcome: &AgentsOutcome, json: bool) -> Option<String> {
    render_agents_with_width(
        outcome,
        json,
        terminal_width(std::io::stdout().is_terminal()),
    )
}

/// `None` preserves complete values for pipes and other non-terminal writers.
fn terminal_width(tty: bool) -> Option<usize> {
    tty.then(|| {
        crossterm::terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80)
    })
}

fn render_agents_with_width(
    outcome: &AgentsOutcome,
    json: bool,
    width: Option<usize>,
) -> Option<String> {
    if json {
        return Some(
            match outcome {
                AgentsOutcome::Status(status) => serde_json::to_string_pretty(&status.overview),
                AgentsOutcome::List(list) => serde_json::to_string_pretty(&list.agents),
                AgentsOutcome::Detail(detail) => serde_json::to_string_pretty(&detail.detail),
                AgentsOutcome::Kill(kill) => serde_json::to_string_pretty(kill),
                AgentsOutcome::Attach(_) => return None,
            }
            .unwrap_or_else(|error| format!("failed to serialize agents outcome: {error}")),
        );
    }
    let rendered = match outcome {
        AgentsOutcome::Status(status) => Some(render_overview(&status.overview, width, None)),
        AgentsOutcome::List(list) => {
            if list.quiet {
                if list.agents.is_empty() {
                    return None;
                }
                return Some(
                    list.agents
                        .iter()
                        .map(|agent| agent.short_id.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            let rows: Vec<Vec<String>> = list
                .agents
                .iter()
                .map(|agent| agent.list_cells(list.no_trunc))
                .collect();
            let rows = if list.no_trunc {
                rows
            } else {
                fit_rows(LIST_COLUMNS, rows, 2, 1, width)
            };
            Some(format_table(LIST_COLUMNS, &rows))
        }
        AgentsOutcome::Detail(detail) => Some(render_detail(detail)),
        AgentsOutcome::Kill(kill) => Some(kill.short_id.clone()),
        AgentsOutcome::Attach(_) => None,
    }
    .map(|text| text.trim_end_matches('\n').to_string());
    rendered
}

fn render_overview(
    overview: &AgentsOverview,
    width: Option<usize>,
    max_rows: Option<usize>,
) -> String {
    let mut out = format!("{}\n\n", overview.header_line());
    if !overview.available {
        out.push_str("Waiting for the runtime to become available.\n");
    } else if overview.agents.is_empty() {
        out.push_str(&overview.empty_state_lines().join("\n"));
        out.push('\n');
    } else {
        let shown = max_rows
            .unwrap_or(overview.agents.len())
            .min(overview.agents.len());
        let rows = overview
            .agents
            .iter()
            .take(shown)
            .map(AgentSnapshot::status_cells)
            .collect();
        let rows = fit_rows(STATUS_COLUMNS, rows, 4, 2, width);
        let mut table = format_table(STATUS_COLUMNS, &rows);
        if width.is_some() {
            table = table
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    if index < 3 || index >= 3 + shown {
                        return line.to_string();
                    }
                    let code = match overview.agents[index - 3].state {
                        AgentState::Running => "32",
                        AgentState::Stuck => "33",
                        AgentState::Waiting => "33",
                        AgentState::Errored => "31",
                        AgentState::Done => "32",
                        AgentState::Stopped => "90",
                    };
                    line.replacen(
                        &format!("│ {} ", overview.agents[index - 3].state.glyph()),
                        &format!(
                            "│ \x1b[{code}m{}\x1b[0m ",
                            overview.agents[index - 3].state.glyph()
                        ),
                        1,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            table.push('\n');
        }
        out.push_str(&table);
        let hidden = overview.agents.len() - shown;
        if hidden > 0 {
            out.push_str(&format!("+{hidden} more\n"));
        }
    }
    out.push_str(&format!("\nTip: {}\n", overview.tip));
    out
}

fn render_detail(detail: &AgentsDetailOutcome) -> String {
    let mut out = String::new();
    for (heading, entries) in detail.groups() {
        out.push_str(heading);
        out.push('\n');
        let rows: Vec<Vec<String>> = entries
            .into_iter()
            .map(|(key, value)| vec![key, value])
            .collect();
        out.push_str(&format_table(&["KEY", "VALUE"], &rows));
        out.push('\n');
    }
    out
}

fn fit_rows(
    headers: &[&str],
    mut rows: Vec<Vec<String>>,
    image_column: usize,
    name_column: usize,
    width: Option<usize>,
) -> Vec<Vec<String>> {
    let Some(width) = width else {
        return rows;
    };
    for column in [image_column, name_column] {
        let widths: Vec<usize> = headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                rows.iter()
                    .map(|row| row[index].width())
                    .max()
                    .unwrap_or(0)
                    .max(header.width())
            })
            .collect();
        let table_width = widths.iter().sum::<usize>() + 3 * widths.len() + 1;
        let overflow = table_width.saturating_sub(width);
        let target = widths[column]
            .saturating_sub(overflow)
            .max(headers[column].width());
        for row in &mut rows {
            row[column] = truncate(&row[column], target);
        }
    }
    rows
}

fn truncate(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width >= width {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}
