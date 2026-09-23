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

use std::collections::BTreeMap;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::{ArgValue, FlagValue, ParsedCommandBoxInput};
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
    /// `status --watch` — what a freshly-opened tab runs over a directory
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
                path: &["status"],
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

        let mut flags: BTreeMap<String, FlagValue> = BTreeMap::new();
        for name in bool_flags {
            assert!(
                spec.find_flag(name).is_some(),
                "frontend action {action:?} sets --{name}, which {path:?} does not declare"
            );
            flags.insert((*name).to_string(), FlagValue::Bool(true));
        }

        let mut arguments: BTreeMap<String, ArgValue> = BTreeMap::new();
        if let Some(value) = argument {
            let arg = spec.arguments.first().unwrap_or_else(|| {
                panic!(
                    "frontend action {action:?} supplies an argument, but {path:?} declares none"
                )
            });
            arguments.insert(arg.name.to_string(), ArgValue::Single(value.to_string()));
        }

        ParsedCommandBoxInput {
            path: path.iter().map(|part| (*part).to_string()).collect(),
            flags,
            arguments,
        }
    }

    /// The invocation a frontend spawns into a freshly-opened tab for
    /// `session`. See [`FrontendAction::startup_for`].
    pub fn startup_command(&'static self, session: &Session) -> ParsedCommandBoxInput {
        self.action_input(FrontendAction::startup_for(session), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch::tests::FakeCommandFrontend;
    use crate::command::dispatch::Dispatch;

    const ALL: &[FrontendAction] = &[
        FrontendAction::ShowConfig,
        FrontendAction::NewSquadTask,
        FrontendAction::EditSquadTask,
        FrontendAction::RemoveSquadTask,
        FrontendAction::TriggerSquadTask,
        FrontendAction::CancelSquadRun,
        FrontendAction::PauseSquadTask,
        FrontendAction::ResumeSquadTask,
        FrontendAction::ReportRepoReadiness,
        FrontendAction::WatchStatus,
    ];

    /// The four squad task actions the TUI's card grid and detail modal
    /// raise, and the `squad` subcommand each is expected to reach.
    const SQUAD_TASK_ACTIONS: &[(FrontendAction, &str)] = &[
        (FrontendAction::TriggerSquadTask, "trigger"),
        (FrontendAction::CancelSquadRun, "cancel"),
        (FrontendAction::PauseSquadTask, "pause"),
        (FrontendAction::ResumeSquadTask, "resume"),
    ];

    /// The guard the hand-built `ParsedCommandBoxInput` literals never had:
    /// every path a frontend can dispatch, and every flag it sets, exists in
    /// the catalogue. Renaming either now fails here instead of at runtime.
    #[test]
    fn frontend_action_paths_and_flags_exist() {
        let catalogue = CommandCatalogue::get();
        for action in ALL {
            let input = catalogue.action_input(*action, None);
            let path: Vec<&str> = input.path.iter().map(String::as_str).collect();
            assert!(
                catalogue.lookup(&path).is_some(),
                "{action:?} dispatches to an unknown command"
            );
        }
    }

    #[test]
    fn a_squad_task_action_carries_the_catalogue_argument_name() {
        let catalogue = CommandCatalogue::get();
        let input = catalogue.action_input(FrontendAction::EditSquadTask, Some("nightly"));
        let spec = catalogue.lookup(&["squad", "edit"]).expect("squad edit");
        let name = spec
            .arguments
            .first()
            .expect("squad edit takes an argument");
        assert_eq!(
            input.arguments.get(name.name),
            Some(&ArgValue::Single("nightly".to_string()))
        );
        assert_eq!(input.flags.get("interview"), Some(&FlagValue::Bool(true)));
    }

    #[test]
    fn a_startup_command_follows_the_session_not_a_dot_git_probe() {
        let catalogue = CommandCatalogue::get();
        let tmp = tempfile::tempdir().expect("scratch dir");

        // A resolved git root: `ready`.
        let repo = Session::for_tests(tmp.path());
        assert!(repo.is_git_repo());
        assert_eq!(
            catalogue.startup_command(&repo).path,
            vec!["ready".to_string()]
        );

        // No git root anywhere: `status --watch`, and the tab still opens.
        let plain = Session::open_or_workdir_fallback(
            tmp.path().to_path_buf(),
            &FailingResolver,
            Default::default(),
        )
        .expect("fall back to the working directory");
        assert!(!plain.is_git_repo());
        let input = catalogue.startup_command(&plain);
        assert_eq!(input.path, vec!["status".to_string()]);
        assert_eq!(input.flags.get("watch"), Some(&FlagValue::Bool(true)));
    }

    struct FailingResolver;
    impl crate::data::session::GitRootResolver for FailingResolver {
        fn resolve(
            &self,
            working_dir: &std::path::Path,
        ) -> Result<std::path::PathBuf, crate::data::error::DataError> {
            Err(crate::data::error::DataError::GitRootNotFound {
                working_dir: working_dir.to_path_buf(),
            })
        }
    }

    /// WI 0114 F-55: the TUI used to dispatch `squad trigger|cancel|pause|
    /// resume` by hand-building a `ParsedCommandBoxInput` whose path and
    /// argument key were frontend literals, so renaming a squad subcommand
    /// left the TUI dispatching a name the catalogue no longer knew and
    /// nothing failed until a user pressed the key.
    ///
    /// The subcommand each action dispatches is asserted to *be* a subcommand
    /// of `squad` in the catalogue, found by walking the catalogue's own
    /// subcommand list: renaming one fails here, and the assertion cannot be
    /// satisfied by a literal the catalogue does not carry.
    #[test]
    fn a_squad_task_action_dispatches_a_catalogue_subcommand() {
        let catalogue = CommandCatalogue::get();
        let squad = catalogue.lookup(&["squad"]).expect("`squad` is a command");
        for (action, subcommand) in SQUAD_TASK_ACTIONS {
            let input = catalogue.action_input(*action, Some("nightly"));
            let path: Vec<&str> = input.path.iter().map(String::as_str).collect();
            assert_eq!(
                path.first(),
                Some(&"squad"),
                "{action:?} must be a squad action"
            );
            let dispatched = path.get(1).unwrap_or_else(|| {
                panic!("{action:?} dispatches {path:?}, which names no subcommand")
            });
            assert_eq!(dispatched, subcommand, "{action:?} changed subcommand");
            let spec = squad
                .subcommands
                .iter()
                .find(|spec| spec.name == *dispatched)
                .unwrap_or_else(|| {
                    panic!("`squad {dispatched}` ({action:?}) is not in the catalogue")
                });
            let arg = spec
                .arguments
                .first()
                .unwrap_or_else(|| panic!("`squad {dispatched}` takes a task name"));
            assert_eq!(
                input.arguments.get(arg.name),
                Some(&ArgValue::Single("nightly".to_string())),
                "{action:?} must key its argument by the catalogue's name"
            );

            // And the whole invocation is the one Dispatch builds from the
            // typed equivalent. Both routes into the same command must agree
            // on the path, every flag and every argument key — a frontend
            // action that drifted from what typing the command does would be
            // a second, divergent spelling of it, which is the shape of
            // F-15, F-21 and F-55 alike.
            //
            // Note on reach (WI 0114 tests-audit): this does *not* catch
            // `action_input` hard-coding the key `"name"`, because every
            // argument-bearing `FrontendAction` targets a `squad` subcommand
            // and `SQUAD_NAME_ARGUMENT` is itself called `name`, so the
            // literal and the catalogue's value coincide. Renaming the
            // argument in the catalogue moves both sides together and nothing
            // observable changes. The first action to take a differently
            // named positional gives the assertion above its teeth; until
            // then only the subcommand half is a live guard, and that half is
            // verified: renaming `trigger` fails this test.
            let typed = Dispatch::<FakeCommandFrontend>::parse_command_box_input(&format!(
                "squad {dispatched} nightly"
            ))
            .unwrap_or_else(|e| panic!("`squad {dispatched} nightly` must parse: {e}"));
            assert_eq!(
                input.path, typed.path,
                "{action:?} dispatches a different path than typing the command"
            );
            assert_eq!(
                input.arguments, typed.arguments,
                "{action:?} keys its arguments differently than typing the command"
            );
        }
    }

    #[test]
    fn removing_a_task_assumes_yes_because_the_dialog_already_asked() {
        let catalogue = CommandCatalogue::get();
        let input = catalogue.action_input(FrontendAction::RemoveSquadTask, Some("nightly"));
        assert_eq!(input.flags.get("yes"), Some(&FlagValue::Bool(true)));
    }
}
