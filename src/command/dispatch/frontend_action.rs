//! Commands a frontend raises on the user's behalf.
//!
//! A key binding, a menu entry and a dialog's confirm button all dispatch a
//! command the user never typed. Before this module each such site hand-built
//! a [`ParsedCommandBoxInput`] with a `path: vec!["squad".into(), …]` and its
//! own flag- and argument-name literals, so nothing was ever checked against
//! the catalogue: renaming `squad add` or `--interview` left the frontend
//! dispatching the old name and failed nothing at compile time.
//!
//! The grand architecture is explicit that the command list "resides within
//! the Dispatch package, NEVER any of the frontend packages". A frontend
//! therefore names the *intent* — a [`FrontendAction`] variant — and Layer 2
//! turns it into an invocation, reading the path, the flag names and the
//! positional argument's name out of the catalogue.

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::projections::raw_args::ParsedArgs;
use crate::data::session::Session;

/// One entry of the action table: the catalogue path an action dispatches to,
/// and the boolean flags the action always carries.
struct ActionSpec {
    path: &'static [&'static str],
    /// Boolean flags set to `true` on every invocation of this action. They
    /// are part of what the action *means*, not a user choice: the TUI's task
    /// interview is `--interview` by definition, and a confirm dialog that has
    /// already asked the question passes the catalogue's "assume yes" flag so
    /// Layer 2 does not ask it twice.
    bool_flags: &'static [&'static str],
}

/// A command a frontend raises itself rather than reading from typed input.
///
/// Each variant is documented with the command line it is equivalent to, so a
/// reader of the frontend still sees what a key does without holding the
/// catalogue in their head — but the strings live here, in Layer 2, where the
/// catalogue can prove them. `frontend_action_paths_and_flags_exist` does
/// exactly that for every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendAction {
    /// `config show` — open the editable configuration table.
    ShowConfig,
    /// `squad add --interview` — create a task through the interview.
    NewSquadTask,
    /// `squad edit <name> --interview` — edit an existing task.
    EditSquadTask,
    /// `squad remove <name> --yes` — delete a task. The dialog that raises
    /// this *is* the confirmation, hence the flag.
    RemoveSquadTask,
    /// `squad trigger <name>` — evaluate a task on the next tick.
    TriggerSquadTask,
    /// `squad cancel <name>` — stop a task's in-progress run.
    CancelSquadRun,
    /// `squad pause <name>` — stop scheduling a task.
    PauseSquadTask,
    /// `squad resume <name>` — put a paused task back on its schedule.
    ResumeSquadTask,
    /// `ready` — what a freshly-opened tab runs over a git repository.
    ReportRepoReadiness,
    /// `agents status --watch` — what a freshly-opened tab runs over a directory
    /// that is not a git repository, where there is nothing to make ready.
    WatchStatus,
}

impl FrontendAction {
    fn spec(self) -> ActionSpec {
        match self {
            FrontendAction::ShowConfig => ActionSpec {
                path: &["config", "show"],
                bool_flags: &[],
            },
            FrontendAction::NewSquadTask => ActionSpec {
                path: &["squad", "add"],
                bool_flags: &["interview"],
            },
            FrontendAction::EditSquadTask => ActionSpec {
                path: &["squad", "edit"],
                bool_flags: &["interview"],
            },
            FrontendAction::RemoveSquadTask => ActionSpec {
                path: &["squad", "remove"],
                bool_flags: &["yes"],
            },
            FrontendAction::TriggerSquadTask => ActionSpec {
                path: &["squad", "trigger"],
                bool_flags: &[],
            },
            FrontendAction::CancelSquadRun => ActionSpec {
                path: &["squad", "cancel"],
                bool_flags: &[],
            },
            FrontendAction::PauseSquadTask => ActionSpec {
                path: &["squad", "pause"],
                bool_flags: &[],
            },
            FrontendAction::ResumeSquadTask => ActionSpec {
                path: &["squad", "resume"],
                bool_flags: &[],
            },
            FrontendAction::ReportRepoReadiness => ActionSpec {
                path: &["ready"],
                bool_flags: &[],
            },
            FrontendAction::WatchStatus => ActionSpec {
                path: &["agents", "status"],
                bool_flags: &["watch"],
            },
        }
    }

    /// What a frontend spawns into a tab the moment it opens.
    ///
    /// Which command that is depends on whether the session is rooted at a
    /// repository — `ready` has nothing to report about a plain directory.
    /// Both the condition and the two command names used to sit in the TUI,
    /// twice over, with the condition spelled as a `.git` probe (WI 0114
    /// F-21).
    pub fn startup_for(session: &Session) -> Self {
        if session.is_git_repo() {
            FrontendAction::ReportRepoReadiness
        } else {
            FrontendAction::WatchStatus
        }
    }

    /// The canonical catalogue path this action dispatches to.
    pub fn path(self) -> &'static [&'static str] {
        self.spec().path
    }
}

impl CommandCatalogue {
    /// Build the invocation for a frontend-raised `action`.
    ///
    /// `argument` fills the command's single positional argument — the
    /// frontend supplies the *value* (a selected task name); its *name* comes
    /// from the command's [`ArgumentSpec`](crate::command::dispatch::catalogue::ArgumentSpec).
    /// Passing a value to a command that declares no positional argument, or
    /// naming a flag the command does not declare, is a programming error and
    /// panics — both are covered for every variant by this module's test.
    ///
    /// [`Dispatch`](crate::command::dispatch::Dispatch) validates the result
    /// exactly as it validates typed input; nothing here bypasses it.
    pub fn action_input(
        &'static self,
        action: FrontendAction,
        argument: Option<&str>,
    ) -> ParsedCommandBoxInput {
        let ActionSpec { path, bool_flags } = action.spec();
        let spec = self.lookup(path).unwrap_or_else(|| {
            panic!("frontend action {action:?} names an unknown command {path:?}")
        });

        for name in bool_flags {
            assert!(
                spec.find_flag(name).is_some(),
                "frontend action {action:?} sets --{name}, which {path:?} does not declare"
            );
        }

        let argument = argument.map(|value| {
            let arg = spec.arguments.first().unwrap_or_else(|| {
                panic!(
                    "frontend action {action:?} supplies an argument, but {path:?} declares none"
                )
            });
            (arg.name, value)
        });

        ParsedCommandBoxInput {
            path: path.iter().map(|part| (*part).to_string()).collect(),
            args: ParsedArgs::for_action(bool_flags, argument),
        }
    }

    /// The invocation a frontend spawns into a freshly-opened tab for
    /// `session`. See [`FrontendAction::startup_for`].
    pub fn startup_command(&'static self, session: &Session) -> ParsedCommandBoxInput {
        self.action_input(FrontendAction::startup_for(session), None)
    }
}
