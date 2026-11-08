//! Background git-diff polling lifecycle for a tab: (re)starting the poll
//! task when the effective working directory changes, and draining
//! stuck/unstuck events for tab coloring.

use super::*;

impl Tab {
    /// (Re)start the git diff poll task against `root`, cancelling any existing
    /// task first. No-ops (leaving the summary untouched) when called outside a
    /// tokio runtime, e.g. in synchronous unit tests.
    pub(super) fn start_git_poll(&mut self, root: std::path::PathBuf) {
        // Cancel and drop the previous task, if any.
        if let Some(cancel) = self.git_poll_cancel.take() {
            cancel.cancel();
        }
        if let Some(handle) = self.git_poll_handle.take() {
            handle.abort();
        }

        // `tokio::spawn` panics without a runtime; skip gracefully so
        // non-async tests can construct tabs.
        if tokio::runtime::Handle::try_current().is_err() {
            self.git_poll_root = Some(root);
            return;
        }

        let cancel = tokio_util::sync::CancellationToken::new();
        let handle =
            self.git_summary
                .start(root.clone(), self.git_diff_summary.clone(), cancel.clone());
        self.git_poll_cancel = Some(cancel);
        self.git_poll_handle = Some(handle);
        self.git_poll_root = Some(root);
    }

    /// Restart the poll task if the tab's effective working directory changed.
    /// The effective root is the active worktree path when set, otherwise the
    /// session git root. Called each tick from `tick_all_tabs`.
    pub fn refresh_git_poll(&mut self) {
        // The squad tab's synthetic session has no meaningful git diff and
        // `new_squad` starts no poll; returning early keeps `tick_all_tabs`
        // from restarting one against the squad storage root.
        if self.is_squad {
            return;
        }
        let desired = self
            .shared
            .workflow
            .snapshot()
            .worktree_path
            .unwrap_or_else(|| self.session.git_root().to_path_buf());
        if self.git_poll_root.as_ref() != Some(&desired) {
            self.start_git_poll(desired);
        }
    }

    /// Drain pending stuck events from the broadcast channel and update
    /// the `stuck` flag for tab coloring.
    pub fn drain_stuck_events(&mut self) {
        for slot in &mut self.container_slots {
            let Some(rx) = slot.stuck_rx.as_mut() else {
                continue;
            };
            let mut events = Vec::new();
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            for event in events {
                match event {
                    StuckEvent::Reported(status) => slot.set_program_status(status),
                    StuckEvent::Stuck => slot.stuck = true,
                    StuckEvent::Unstuck | StuckEvent::StartupGraceExpired => slot.stuck = false,
                }
            }
        }
        self.stuck = self.container_slots.iter().any(|slot| slot.stuck);
        let status = self.program_status();
        self.shared.signals.set_status(status);

        // WI-0096 §9: while a parallel group is active (the sequential
        // backbone is stashed in `dormant_slots`), aggregate the stuck /
        // yolo indicators across the group's slots for tab coloring.
        // Outside a group the broadcast channel above and the spawn-time
        // yolo flag drive the indicators, exactly as for plain commands.
        if !self.dormant_slots.is_empty() {
            self.yolo_mode = self.container_slots.iter().any(|s| s.yolo_mode);
        }
    }
}
