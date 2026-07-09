//! Container-slot lifecycle: slot creation/eviction, the parallel-group
//! shared-event queue, PTY output draining, and Ctrl-S slot cycling.

use super::*;

impl Tab {
    /// Number of active (non-exited) container slots. `1` for plain
    /// containerized commands, `N` during a parallel workflow group, `0`
    /// while nothing containerized runs.
    pub fn active_slot_count(&self) -> usize {
        self.container_slots.len()
    }

    /// Whether the tab is showing more than one container. Ctrl-S slot
    /// switching only activates here.
    pub fn has_multiple_slots(&self) -> bool {
        self.container_slots.len() > 1
    }

    /// Whether the container overlay is currently covering the execution
    /// window: a slot exists and the display is Maximized. Minimized shows
    /// every slot as a status bar (no overlay); Hidden shows nothing. Key
    /// routing, mouse routing, and the execution-window renderer must all
    /// agree with `render_frame` on this.
    pub fn container_overlay_active(&self) -> bool {
        self.container_window_state == ContainerWindowState::Maximized
            && !self.container_slots.is_empty()
    }

    /// Mutable access to the currently-focused container slot. `None` while
    /// nothing containerized is running.
    pub fn focused_slot_mut(&mut self) -> Option<&mut ContainerSlot> {
        self.container_slots.get_mut(self.focused_slot_idx)
    }

    /// Immutable counterpart to [`focused_slot_mut`](Self::focused_slot_mut).
    pub fn focused_slot(&self) -> Option<&ContainerSlot> {
        self.container_slots.get(self.focused_slot_idx)
    }

    /// Focused slot's vt100 parser. Panics when no slot exists — test-only
    /// convenience for feeding PTY bytes and probing the grid.
    #[cfg(test)]
    pub fn focused_parser_mut(&mut self) -> &mut vt100::Parser {
        let idx = self.focused_slot_idx;
        &mut self.container_slots[idx].vt100_parser
    }

    /// Drain the shared parallel-slot event queue and update `container_slots`
    /// accordingly (WI-0096 §12). Events are processed in the order the engine
    /// emitted them, so an "exited then dequeued" pair in the same tick evicts
    /// the old slot before adding the new one — keeping the net slot count
    /// correct.
    pub fn drain_container_slot_events(&mut self) {
        let events: Vec<ContainerSlotEvent> = match self.container_slot_events.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return,
        };
        for event in events {
            self.apply_container_slot_event(event);
        }
    }

    fn apply_container_slot_event(&mut self, event: ContainerSlotEvent) {
        match event {
            ContainerSlotEvent::GroupStarted => {
                // Stash the sequential backbone slot(s) for the duration of
                // the group. Their stdout receivers must stay alive: later
                // sequential steps send through the same persistent channel.
                self.dormant_slots.append(&mut self.container_slots);
                self.focused_slot_idx = 0;
            }
            ContainerSlotEvent::Launched {
                step_name,
                agent,
                io,
                ..
            } => {
                if self
                    .container_slots
                    .iter()
                    .any(|s| s.step_name == step_name)
                {
                    return;
                }
                let scrollback = self.session.effective_config().scrollback_lines();
                let mut slot = ContainerSlot::new(step_name, agent, scrollback);
                if let Some(io) = io {
                    slot.container_stdout_rx = Some(io.stdout_rx);
                    slot.container_stdin_tx = Some(io.stdin_tx);
                    slot.container_resize_tx = Some(io.resize_tx);
                }
                // Size the fresh 80x24 parser to the real overlay dimensions
                // immediately and push the size to the container's PTY, so
                // the agent never lays out against the default grid. The
                // per-tick sync in `tick_all_tabs` keeps it correct after.
                let size = self
                    .container_inner_area
                    .map(|r| (r.width, r.height))
                    .or_else(|| {
                        crossterm::terminal::size().ok().map(|(tc, tr)| {
                            let sidebar = crate::frontend::tui::git_sidebar::sidebar_width(
                                tc,
                                self.git_sidebar_state,
                            );
                            crate::frontend::tui::event_loop::compute_container_inner_size(
                                tc.saturating_sub(sidebar),
                                tr,
                            )
                        })
                    });
                if let Some((cols, rows)) = size {
                    slot.vt100_parser.screen_mut().set_size(rows, cols);
                    if let Some(ref tx) = slot.container_resize_tx {
                        let _ = tx.send((cols, rows));
                    }
                }
                self.container_slots.push(slot);
            }
            ContainerSlotEvent::ContainerName {
                step_name,
                container_name,
            } => {
                if let Some(info) = self
                    .slot_mut(&step_name)
                    .and_then(|s| s.container_info.as_mut())
                {
                    info.container_name = container_name;
                    info.latest_stats = None;
                }
            }
            ContainerSlotEvent::Exited { step_name } => self.evict_slot(&step_name),
            ContainerSlotEvent::Stuck { step_name } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.stuck = true;
                }
            }
            ContainerSlotEvent::Unstuck { step_name } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.stuck = false;
                }
            }
            ContainerSlotEvent::YoloStarted { step_name } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.yolo_mode = true;
                }
            }
            ContainerSlotEvent::YoloFinished { step_name } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.yolo_mode = false;
                }
            }
            ContainerSlotEvent::GroupFinished => {
                // Drop the group's slots and restore the sequential backbone
                // so the next sequential step's output (sent through the
                // persistent command-level channel) has a slot to land in.
                self.container_slots.clear();
                self.container_slots.append(&mut self.dormant_slots);
                self.focused_slot_idx = 0;
            }
        }
    }

    /// Drain pending PTY output from every slot into its vt100 parser.
    ///
    /// Auto-opens the container overlay to Maximized the first time bytes
    /// arrive so the user sees the PTY output immediately without having to
    /// manually cycle with Ctrl+M.
    ///
    /// Between sequential workflow steps the engine sets `pty_reset_flag`,
    /// which reinitializes the focused slot's parser (clearing the previous
    /// step's terminal content) before the new step's output is processed.
    pub fn drain_container_output(&mut self) {
        if self.pty_reset_flag.swap(false, Ordering::Relaxed) {
            let scrollback = self.session.effective_config().scrollback_lines();
            let focused_idx = self.focused_slot_idx;
            if let Some(slot) = self.container_slots.get_mut(focused_idx) {
                let (rows, cols) = slot.vt100_parser.screen().size();
                slot.vt100_parser = vt100::Parser::new(rows, cols, scrollback);
                slot.agent_alt_screen = false;
                slot.agent_alternate_scroll = false;
                slot.region_scroll.reset();
            }
            self.container_scroll_offset = 0;
            self.container_rendered = false;
            self.mouse_selection = None;
            // A new step's container is launching — allow auto-open again.
            self.suppress_container_auto_open = false;
        }

        let mut received_any = false;
        for slot in &mut self.container_slots {
            let Some(rx) = slot.container_stdout_rx.as_mut() else {
                continue;
            };
            while let Ok(bytes) = rx.try_recv() {
                let filtered = strip_alternate_screen_sequences(&bytes);
                if let Some(on) = filtered.alt_screen {
                    slot.agent_alt_screen = on;
                }
                if let Some(on) = filtered.alternate_scroll {
                    slot.agent_alternate_scroll = on;
                }
                slot.region_scroll
                    .process(&mut slot.vt100_parser, &filtered.bytes);
                received_any = true;
            }
        }
        if received_any
            && !self.suppress_container_auto_open
            && self.container_window_state == ContainerWindowState::Hidden
        {
            self.container_window_state = ContainerWindowState::Maximized;
        }
    }

    fn slot_mut(&mut self, step_name: &str) -> Option<&mut ContainerSlot> {
        self.container_slots
            .iter_mut()
            .find(|s| s.step_name == step_name)
    }

    /// Remove the slot for `step_name` with NO grey summary bar (WI-0096 §12)
    /// and recompute `focused_slot_idx`: if a slot before the focused one is
    /// removed, shift the index down; if the focused slot itself exits, advance
    /// to the next live slot (wrapping). When no slots remain, reset the index
    /// and let the container window go Hidden.
    fn evict_slot(&mut self, step_name: &str) {
        let Some(pos) = self
            .container_slots
            .iter()
            .position(|s| s.step_name == step_name)
        else {
            return;
        };
        self.container_slots.remove(pos);
        let len = self.container_slots.len();
        if len == 0 {
            self.focused_slot_idx = 0;
            self.container_window_state = ContainerWindowState::Hidden;
            return;
        }
        if pos < self.focused_slot_idx {
            self.focused_slot_idx -= 1;
        } else if pos == self.focused_slot_idx {
            // Removing shifts the next live slot into `pos`; wrap if it was the
            // last slot.
            if self.focused_slot_idx >= len {
                self.focused_slot_idx = 0;
            }
        }
    }

    /// Advance `focused_slot_idx` to the next active slot (WI-0096 §6, Ctrl-S).
    /// No-op with zero or one slot. Returns the newly-focused slot's resize
    /// sender (if any) so the caller can push the current terminal size to it.
    pub fn cycle_focused_slot(&mut self) {
        if self.container_slots.len() <= 1 {
            return;
        }
        self.focused_slot_idx = (self.focused_slot_idx + 1) % self.container_slots.len();
        self.mouse_selection = None;
        // The scrollback offset belongs to the overlay's current content;
        // start the rotated-in slot at its live view.
        self.container_scroll_offset = 0;
        // Un-minimize so the rotated container becomes visible.
        if self.container_window_state == ContainerWindowState::Minimized {
            self.container_window_state = ContainerWindowState::Maximized;
        }
    }

    /// Install a fresh single container slot for a newly-spawned command,
    /// replacing any previous slots. The slot's parser is sized to
    /// `cols`x`rows` (the computed overlay inner size); I/O channels are
    /// attached by the caller via [`focused_slot_mut`](Self::focused_slot_mut).
    ///
    /// This is the N==1 case of the unified slot model: a plain containerized
    /// command is simply a one-slot group.
    pub fn start_container(
        &mut self,
        agent_display_name: String,
        container_name: String,
        cols: u16,
        rows: u16,
    ) {
        self.container_scroll_offset = 0;
        self.container_rendered = false;
        self.last_container_summary = None;
        self.mouse_selection = None;
        let scrollback = self.session.effective_config().scrollback_lines();
        let mut slot = ContainerSlot::new(String::new(), agent_display_name, scrollback);
        slot.vt100_parser.screen_mut().set_size(rows, cols);
        if let Some(info) = slot.container_info.as_mut() {
            info.container_name = container_name;
        }
        self.container_slots.clear();
        self.dormant_slots.clear();
        self.focused_slot_idx = 0;
        self.container_slots.push(slot);
    }


    /// Drop every container slot (active and dormant) and the command result
    /// channel. Called when the command task is over in any way — the tab
    /// returns to the no-container state.
    pub(super) fn clear_container_slots(&mut self) {
        self.container_slots.clear();
        self.dormant_slots.clear();
        self.focused_slot_idx = 0;
        self.command_result_rx = None;
    }
}
