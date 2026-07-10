//! Container overlay teardown and command-completion polling: surfacing
//! output from fast-failing launches, closing the overlay (with the
//! post-exit summary bar), and reacting to container/command exit.

use super::*;

impl Tab {
    /// Push the container's terminal contents to the status log when the
    /// overlay never got a frame on screen (the agent exited within one
    /// event-loop tick of producing its first output). Without this, a
    /// fast-failing launch's error output is parsed into the vt100 grid and
    /// then discarded before the user ever sees it.
    fn surface_unseen_container_output(&mut self) {
        if self.container_rendered {
            return;
        }
        let Some(slot) = self.focused_slot() else {
            return;
        };
        let contents = slot.vt100_parser.screen().contents();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return;
        }
        if let Ok(mut log) = self.status_log.lock() {
            log.push(crate::frontend::tui::user_message::StatusLogEntry {
                level: crate::data::message::MessageLevel::Warning,
                text: "Agent exited before its output could be displayed; captured output:"
                    .to_string(),
            });
            for line in lines {
                log.push(crate::frontend::tui::user_message::StatusLogEntry {
                    level: crate::data::message::MessageLevel::Info,
                    text: format!("  {line}"),
                });
            }
        }
    }

    /// Tear down the container overlay state. Called when a containerized
    /// command finishes (exit, error, or task drop). Captures
    /// `LastContainerSummary` from the focused slot's `ContainerInfo` so the
    /// post-exit summary bar can show averaged stats and the exit code. The
    /// slot itself is left in place — mid-workflow closes reuse its info for
    /// the next step's stats polling and title; `poll_command_completion`
    /// clears the slots when the whole command is over.
    fn close_container_overlay(&mut self, exit_code: i32) {
        self.surface_unseen_container_output();
        if self.container_window_state != ContainerWindowState::Hidden {
            if let Some(info) = self.focused_slot().and_then(|s| s.container_info.as_ref()) {
                let elapsed = info.start_time.elapsed().as_secs();
                let (avg_cpu, avg_memory) = if info.stats_history.is_empty() {
                    ("n/a".to_string(), "n/a".to_string())
                } else {
                    let count = info.stats_history.len() as f64;
                    let cpu_avg: f64 =
                        info.stats_history.iter().map(|(c, _)| c).sum::<f64>() / count;
                    let mem_avg: f64 =
                        info.stats_history.iter().map(|(_, m)| m).sum::<f64>() / count;
                    (format!("{:.1}%", cpu_avg), format!("{:.0}MiB", mem_avg))
                };
                self.last_container_summary = Some(LastContainerSummary {
                    agent_display_name: info.agent_display_name.clone(),
                    container_name: info.container_name.clone(),
                    avg_cpu,
                    avg_memory,
                    total_time: format_duration(elapsed),
                    exit_code,
                });
            }
        }
        self.container_window_state = ContainerWindowState::Hidden;
        self.container_inner_area = None;
        self.mouse_selection = None;
        self.container_scroll_offset = 0;
        for slot in &mut self.container_slots {
            slot.agent_alt_screen = false;
            slot.agent_alternate_scroll = false;
        }
        self.stuck = false;
        self.stuck_rx = None;
    }

    /// Close the container window as soon as the engine reports that a
    /// mid-workflow container has actually terminated (killed by awman —
    /// e.g. after a yolo countdown — or the user quit the agent process).
    ///
    /// The engine only publishes the exit code on real container death,
    /// never for a stuck-but-alive container or while a yolo countdown is
    /// still running, so taking the slot is the whole gate. The summary bar
    /// is left behind (captured by `close_container_overlay`), and the slot
    /// (with its `ContainerInfo`) is kept alive because later workflow steps
    /// reuse it for stats polling and the overlay title.
    pub fn poll_container_exit(&mut self) {
        let exit_code = self
            .container_exit_shared
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        let Some(exit_code) = exit_code else {
            return;
        };
        // Pull any final bytes into the parser first so the captured
        // scrollback (and `surface_unseen_container_output`) is complete.
        self.drain_container_output();
        self.close_container_overlay(exit_code);
        // Late bytes still in flight from the dead container must not
        // re-open the window; cleared when the next container launches.
        self.suppress_container_auto_open = true;
    }

    /// Check if the command task has completed; update execution phase.
    ///
    /// Closes the container overlay on completion so the user regains full
    /// keyboard control without having to manually cycle Ctrl+M.
    pub fn poll_command_completion(&mut self) {
        if let Some(ref rx) = self.command_result_rx {
            match rx.try_recv() {
                Ok(Ok(outcome)) => {
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    // Agent-session commands carry the agent's real exit code;
                    // reflect it instead of unconditionally reporting success.
                    let exit_code = match &outcome {
                        CommandOutcome::Chat(o) => o.exit_code.unwrap_or(0),
                        CommandOutcome::ExecPrompt(o) => o.exit_code.unwrap_or(0),
                        _ => 0,
                    };
                    if let Ok(mut log) = self.status_log.lock() {
                        if exit_code == 0 {
                            log.push(crate::frontend::tui::user_message::StatusLogEntry {
                                level: crate::data::message::MessageLevel::Success,
                                text: format!("Command '{}' completed successfully.", cmd_name),
                            });
                        } else {
                            log.push(crate::frontend::tui::user_message::StatusLogEntry {
                                level: crate::data::message::MessageLevel::Error,
                                text: format!(
                                    "Command '{}' finished: agent exited with code {}.",
                                    cmd_name, exit_code
                                ),
                            });
                        }
                    }
                    self.execution_phase = ExecutionPhase::Done {
                        command: cmd_name,
                        exit_code,
                    };
                    self.close_container_overlay(exit_code);
                    // The command is over — drop every slot (and any dormant
                    // backbone) so the tab returns to the no-container state.
                    self.clear_container_slots();
                }
                Ok(Err(err)) => {
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    let err_msg = format!("{err}");
                    if let Ok(mut log) = self.status_log.lock() {
                        log.push(crate::frontend::tui::user_message::StatusLogEntry {
                            level: crate::data::message::MessageLevel::Error,
                            text: format!("Command '{}' failed: {}", cmd_name, err_msg),
                        });
                    }
                    self.execution_phase = ExecutionPhase::Error {
                        command: cmd_name,
                        message: err_msg,
                    };
                    self.close_container_overlay(-1);
                    self.clear_container_slots();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still running — nothing to do.
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Command task dropped without sending a result — in
                    // practice this means the task panicked. The panic hook
                    // records the backtrace; point the user at it.
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    let err_msg = match crate::frontend::tui::event_loop::panic_log_path() {
                        Some(path) => format!(
                            "command task ended unexpectedly (likely a panic — see {})",
                            path.display()
                        ),
                        None => "command task ended unexpectedly (likely a panic)".to_string(),
                    };
                    if let Ok(mut log) = self.status_log.lock() {
                        log.push(crate::frontend::tui::user_message::StatusLogEntry {
                            level: crate::data::message::MessageLevel::Error,
                            text: format!("Command '{}' failed: {}", cmd_name, err_msg),
                        });
                    }
                    self.execution_phase = ExecutionPhase::Error {
                        command: cmd_name,
                        message: err_msg,
                    };
                    self.close_container_overlay(-1);
                    self.clear_container_slots();
                }
            }
        }
    }
}
