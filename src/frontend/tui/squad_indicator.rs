//! The bottom-row squad health indicator (WI 0112 Part 2).
//!
//! An app-level poller — independent of the squad tab, which may never have
//! been opened, and of the tab's own `SquadTaskPoller`, which fetches only
//! while that tab is focused — probes the squad daemon every ten seconds and
//! publishes one of seven states. The renderer paints a coloured `●` for it on
//! every tab. The poller holds no policy: [`classify`] is the whole decision
//! and is a pure function.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::supervisor::SquadGatewayResolver;
use crate::command::error::CommandError;
use crate::data::config::env::Env;
use crate::data::fs::task_store::{RunStatus, Task};

/// How often the indicator re-probes the daemon.
pub const SQUAD_INDICATOR_INTERVAL: Duration = Duration::from_secs(10);
/// How long one probe may take before it is reported as unreachable. Keeps a
/// hung daemon from stacking probes: at most one is ever in flight.
pub const SQUAD_INDICATOR_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What the indicator reports. Ordered by precedence, most severe first —
/// [`classify`] returns the first that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SquadIndicator {
    /// No probe has completed yet. Rendered like `NotRunning`.
    Unknown,
    /// No squad daemon process is running (pidfile check).
    NotRunning,
    /// A daemon is running but this TUI got no successful answer from it: no
    /// endpoint sidecar, no bearer key (a 401), connection refused, timeout,
    /// or any other transport or HTTP error.
    Unreachable,
    /// Reachable, and at least one task's most recent run failed.
    Failed,
    /// Reachable, nothing failed, and at least one task declares an `env()`
    /// variable the daemon has no value for (WI 0116 §6c).
    ///
    /// This is *per-task* coverage and nothing else. In particular a keychain
    /// **persistence fallback never lands here** — see [`classify`].
    EnvUnmet,
    /// Reachable, and at least one task is executing right now.
    Running,
    /// Reachable, nothing failed, nothing running.
    Healthy,
}

/// Cross-thread handle: the poller writes, the renderer reads.
pub type SharedSquadIndicator = Arc<Mutex<SquadIndicator>>;

/// Map one probe's result onto an indicator state. `daemon_running` is the
/// pidfile answer; `probe` is the task list the daemon returned, or why it
/// did not. Pure, so every row of the state table is unit-tested without a
/// daemon.
///
/// Precedence is `Unreachable` > `Failed` > `EnvUnmet` > `Running` >
/// `Healthy`. `EnvUnmet` beats blue for the same reason red does — an unmet
/// variable is persistent and actionable, while a running task is transient —
/// and loses to red because a failed run is the more urgent fact, and is
/// frequently *caused* by the unmet variable the user will meet on arriving at
/// the tab either way.
///
/// **Deliberately not here: the keychain-persistence fallback.** On headless
/// Linux and on Windows that fallback is the expected steady state, so wiring
/// it in would pin the indicator yellow forever on those platforms — and an
/// indicator that is always yellow teaches users to ignore it, which costs more
/// than the warning gains. Persistence state belongs in `squad status` and
/// `awman squad env`, where it is sought deliberately; this indicator is
/// reserved for per-task unmet variables, a condition that is always someone's
/// to fix and that goes away when they fix it. `DaemonStatus.env_persistence`
/// is not a parameter of this function for exactly that reason — please do not
/// "fix" it by adding one.
pub fn classify(daemon_running: bool, probe: Result<&[Task], &CommandError>) -> SquadIndicator {
    if !daemon_running {
        return SquadIndicator::NotRunning;
    }
    let Ok(tasks) = probe else {
        return SquadIndicator::Unreachable;
    };
    // Red beats blue: a failure needs attention and persists; a running task
    // is transient and shows once the failure is cleared or another run
    // starts.
    if tasks
        .iter()
        .any(|task| task.last_run_status == Some(RunStatus::Failed))
    {
        return SquadIndicator::Failed;
    }
    // Yellow beats blue, for the same reason red does. The data rides on the
    // task list this poller already fetches, so the seventh state costs no
    // second round trip.
    if tasks.iter().any(|task| !task.unmet_env.is_empty()) {
        return SquadIndicator::EnvUnmet;
    }
    if tasks
        .iter()
        .any(|task| task.last_run_status == Some(RunStatus::Running))
    {
        return SquadIndicator::Running;
    }
    SquadIndicator::Healthy
}

/// The background probe. Started once per TUI process from `tui::run`, never
/// from `App::new`, so unit-test apps never touch `~/.awman/squad`.
pub struct SquadIndicatorPoller {
    shared: SharedSquadIndicator,
}

impl SquadIndicatorPoller {
    pub fn new(shared: SharedSquadIndicator) -> Self {
        Self { shared }
    }

    /// Ticks every [`SQUAD_INDICATOR_INTERVAL`] until `cancel` fires. The
    /// first probe runs immediately so the indicator leaves `Unknown` on the
    /// first tick rather than ten seconds in.
    pub fn start(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SQUAD_INDICATOR_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let state = tokio::time::timeout(SQUAD_INDICATOR_PROBE_TIMEOUT, probe_once())
                            .await
                            .unwrap_or(SquadIndicator::Unreachable);
                        if let Ok(mut guard) = self.shared.lock() {
                            *guard = state;
                        }
                    }
                }
            }
        })
    }
}

/// One probe: pidfile, then a keyless-or-existing-key gateway, then `list`.
/// `Env` is re-read every time so a key minted mid-session (published into
/// the process environment by the supervisor) is picked up without a restart.
async fn probe_once() -> SquadIndicator {
    let supervisor = match SquadGatewayResolver::from_env(&Env::from_process()) {
        Ok(supervisor) => supervisor,
        Err(_) => return SquadIndicator::Unreachable,
    };
    match supervisor.daemon_is_running() {
        Ok(true) => {}
        Ok(false) => return SquadIndicator::NotRunning,
        Err(_) => return SquadIndicator::Unreachable,
    }
    let gateway = match supervisor.probe_gateway() {
        Ok(Some(gateway)) => gateway,
        Ok(None) | Err(_) => return SquadIndicator::Unreachable,
    };
    match gateway.list().await {
        Ok(tasks) => classify(true, Ok(&tasks)),
        Err(error) => classify(true, Err(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fs::task_store::{MountScope, TaskStatus};
    use chrono::Utc;
    use std::path::PathBuf;

    fn task(name: &str, last_run_status: Option<RunStatus>) -> Task {
        let now = Utc::now();
        Task {
            id: name.to_string(),
            name: name.to_string(),
            description: "test".into(),
            repo_scope: PathBuf::from("/workspace"),
            mount_scope: MountScope::Directory,
            overlays: Vec::new(),
            interval_secs: 60,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status,
            unmet_env: Vec::new(),
        }
    }

    #[test]
    fn a_daemon_that_is_not_running_is_grey_whatever_the_probe_says() {
        let tasks = [task("a", Some(RunStatus::Failed))];
        assert_eq!(classify(false, Ok(&tasks)), SquadIndicator::NotRunning);
        assert_eq!(
            classify(false, Err(&CommandError::RemoteTimeout)),
            SquadIndicator::NotRunning
        );
    }

    #[test]
    fn every_probe_error_is_unreachable() {
        for error in [
            CommandError::RemoteTimeout,
            CommandError::RemoteConnectionRefused("refused".into()),
            CommandError::RemoteHttpStatus {
                status: 401,
                body: "Invalid API key.".into(),
            },
            CommandError::RemoteHttpStatus {
                status: 500,
                body: "boom".into(),
            },
            CommandError::RemoteTransport("eof".into()),
        ] {
            assert_eq!(
                classify(true, Err(&error)),
                SquadIndicator::Unreachable,
                "{error}"
            );
        }
    }

    #[test]
    fn a_reachable_daemon_with_no_tasks_is_healthy() {
        assert_eq!(classify(true, Ok(&[])), SquadIndicator::Healthy);
    }

    #[test]
    fn a_failed_last_run_is_red_and_beats_a_running_task() {
        let tasks = [
            task("a", Some(RunStatus::Running)),
            task("b", Some(RunStatus::Failed)),
        ];
        assert_eq!(classify(true, Ok(&tasks)), SquadIndicator::Failed);
    }

    #[test]
    fn a_running_task_is_blue() {
        let tasks = [
            task("a", Some(RunStatus::WorkflowExecuted)),
            task("b", Some(RunStatus::Running)),
        ];
        assert_eq!(classify(true, Ok(&tasks)), SquadIndicator::Running);
    }

    #[test]
    fn interrupted_and_ordinary_outcomes_are_healthy() {
        let tasks = [
            task("a", Some(RunStatus::Interrupted)),
            task("b", Some(RunStatus::NotTriggered)),
            task("c", Some(RunStatus::WorkflowExecuted)),
            task("d", None),
        ];
        assert_eq!(classify(true, Ok(&tasks)), SquadIndicator::Healthy);
    }

    /// A task with `unmet_env` set — the WI 0116 §6c input. The unmet names
    /// ride on the very `Task` the poller's one `list` call already returns.
    fn task_with_unmet(name: &str, last_run_status: Option<RunStatus>, unmet: &[&str]) -> Task {
        Task {
            unmet_env: unmet.iter().map(|s| s.to_string()).collect(),
            ..task(name, last_run_status)
        }
    }

    /// WI 0116 §6c: the seventh state. One task with an uncovered `env()` name
    /// is enough, whatever the rest of the grid looks like.
    #[test]
    fn a_task_with_an_unmet_env_name_is_env_unmet() {
        let tasks = [
            task("clean", Some(RunStatus::WorkflowExecuted)),
            task_with_unmet("deploy", None, &["AWS_PROFILE"]),
        ];
        assert_eq!(classify(true, Ok(&tasks)), SquadIndicator::EnvUnmet);
    }

    /// Red still beats yellow: a failed run is the more urgent fact, and it is
    /// frequently *caused* by the unmet variable the user meets on arriving at
    /// the tab either way.
    #[test]
    fn a_failed_task_with_an_unmet_env_name_is_still_failed() {
        let tasks = [task_with_unmet(
            "deploy",
            Some(RunStatus::Failed),
            &["AWS_PROFILE"],
        )];
        assert_eq!(classify(true, Ok(&tasks)), SquadIndicator::Failed);
        // …including when the failure and the unmet name are on different
        // tasks, which is the shape the precedence table actually describes.
        let split = [
            task("other", Some(RunStatus::Failed)),
            task_with_unmet("deploy", None, &["AWS_PROFILE"]),
        ];
        assert_eq!(classify(true, Ok(&split)), SquadIndicator::Failed);
    }

    /// **The precedence change, and the row most likely to be got backwards.**
    /// Yellow beats blue for the same reason red does: an unmet variable is
    /// persistent and actionable, a running task is transient and will show
    /// itself on the next tick.
    #[test]
    fn a_running_task_with_an_unmet_env_name_is_env_unmet_not_running() {
        let same_task = [task_with_unmet(
            "deploy",
            Some(RunStatus::Running),
            &["AWS_PROFILE"],
        )];
        assert_eq!(
            classify(true, Ok(&same_task)),
            SquadIndicator::EnvUnmet,
            "an unmet variable outranks a run in flight"
        );
        let split = [
            task("busy", Some(RunStatus::Running)),
            task_with_unmet("deploy", None, &["AWS_PROFILE"]),
        ];
        assert_eq!(classify(true, Ok(&split)), SquadIndicator::EnvUnmet);
    }

    /// An unreachable daemon outranks everything reachable, including the new
    /// state: a daemon that cannot answer cannot have reported coverage, so a
    /// yellow `EnvUnmet` would be describing stale data.
    #[test]
    fn an_unreachable_daemon_outranks_env_unmet_whatever_the_last_answer_said() {
        for error in [
            CommandError::RemoteTimeout,
            CommandError::RemoteConnectionRefused("refused".into()),
            CommandError::RemoteHttpStatus {
                status: 401,
                body: "Invalid API key.".into(),
            },
        ] {
            assert_eq!(
                classify(true, Err(&error)),
                SquadIndicator::Unreachable,
                "{error}"
            );
        }
        // And a daemon that is not running at all is grey, not yellow.
        let tasks = [task_with_unmet("deploy", None, &["AWS_PROFILE"])];
        assert_eq!(classify(false, Ok(&tasks)), SquadIndicator::NotRunning);
    }

    /// **The deliberate exclusion in §6c, asserted so nobody "fixes" it.**
    ///
    /// A keychain-persistence fallback is not an input to this function at all
    /// — `DaemonStatus.env_persistence` is not a parameter — so a daemon that
    /// cannot reach a keychain, with every task fully covered, stays `Healthy`.
    /// On headless Linux and on Windows that fallback is the *expected steady
    /// state*; wiring it in here would pin the indicator yellow forever on
    /// those platforms, and an always-yellow indicator teaches users to ignore
    /// it. Persistence state belongs in `squad status` and `awman squad env`,
    /// where it is sought deliberately.
    #[test]
    fn a_persistence_fallback_with_no_unmet_task_variable_leaves_the_indicator_healthy() {
        // Exactly the state a headless Linux box is in: the daemon reports
        // `unavailable(secret-tool not found)` on `/v1/status`, and every task
        // has its values because the shell pushed them this session.
        let tasks = [
            task_with_unmet("nightly", Some(RunStatus::WorkflowExecuted), &[]),
            task_with_unmet("deploy", None, &[]),
        ];
        assert_eq!(
            classify(true, Ok(&tasks)),
            SquadIndicator::Healthy,
            "a persistence fallback must never colour this indicator"
        );
    }

    /// The data rides on the task list the poller already fetches: an empty
    /// `unmet_env` is the same input shape as before WI 0116, and every
    /// pre-existing row of the table still answers the same way.
    #[test]
    fn an_empty_unmet_env_changes_none_of_the_pre_wi_0116_rows() {
        assert_eq!(classify(true, Ok(&[])), SquadIndicator::Healthy);
        assert_eq!(
            classify(
                true,
                Ok(&[task_with_unmet("a", Some(RunStatus::Running), &[])])
            ),
            SquadIndicator::Running
        );
        assert_eq!(
            classify(
                true,
                Ok(&[task_with_unmet("a", Some(RunStatus::Failed), &[])])
            ),
            SquadIndicator::Failed
        );
    }
}
