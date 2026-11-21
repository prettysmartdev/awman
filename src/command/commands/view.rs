//! `awman view --instance <ins_…> [--tab <tab_…>] [--annotation k=v]...`
//! (WI 0142 Part B §2 "Hidden entry point"): a secondary TUI frontend rendering
//! one tab (or all tabs) of an existing daemon-hosted instance. Used by host
//! presenters; marked `internal` in the catalogue and absent from `--help` and
//! the docs.
//!
//! Layer 2 owns the arguments (`ViewArgs`) and the handler (`ViewCommand`).
//! The view itself is a TUI, which Layer 2 cannot name: the handler calls the
//! [`ViewEntry`] its caller supplies. The TUI implements it by running
//! `tui::run` with `InitialTab::View(args)` over a secondary
//! `RemoteInstanceBackend` (design §4.10/§4.11). `Dispatch` never runs `view`:
//! its catalogue builder refuses, so the command box cannot open a view
//! inside a view.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.10 (D10).

use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::annotations::Annotations;
use crate::data::ids::{InstanceId, TabId};

/// The command name `main.rs` routes to the TUI view entry point.
pub const VIEW_COMMAND: &str = "view";
pub const FLAG_INSTANCE: &str = "instance";
pub const FLAG_TAB: &str = "tab";
pub const FLAG_ANNOTATION: &str = "annotation";

/// The parsed `view` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewArgs {
    pub instance: InstanceId,
    /// The one tab to render; every tab when `None`.
    pub tab: Option<TabId>,
    /// This view's annotations (sent in its `Hello`).
    pub annotations: Annotations,
}

impl ViewArgs {
    /// From resolved catalogue flags.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let instance = ctx.flags.require_str(FLAG_INSTANCE)?;
        let tab = ctx.flags.string(FLAG_TAB);
        Self::from_flags(&instance, tab.as_deref(), ctx.flags.strs(FLAG_ANNOTATION))
    }

    /// From raw flag values (the binary's clap matches).
    pub fn from_flags(
        instance: &str,
        tab: Option<&str>,
        annotations: &[String],
    ) -> Result<Self, CommandError> {
        let instance = instance.parse::<InstanceId>().map_err(|_| {
            invalid(
                FLAG_INSTANCE,
                format!("'{instance}' is not an instance id (ins_…)"),
            )
        })?;
        let tab = tab
            .map(|tab| {
                tab.parse::<TabId>()
                    .map_err(|_| invalid(FLAG_TAB, format!("'{tab}' is not a tab id (tab_…)")))
            })
            .transpose()?;
        let annotations = Annotations::parse_cli(annotations)
            .map_err(|error| invalid(FLAG_ANNOTATION, error.to_string()))?;
        Ok(Self {
            instance,
            tab,
            annotations,
        })
    }
}

fn invalid(flag: &str, reason: String) -> CommandError {
    CommandError::InvalidFlagValue {
        command: vec!["view".into()],
        flag: flag.into(),
        reason,
    }
}

/// The Layer 3 entry point a `view` runs through. Implemented by the TUI
/// (`tui::run` with `InitialTab::View`); returns the process exit code.
pub trait ViewEntry {
    fn open_view(&self, args: ViewArgs) -> Result<i32, CommandError>;
}

/// The `view` handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewCommand {
    args: ViewArgs,
}

impl ViewCommand {
    pub fn new(args: ViewArgs) -> Self {
        Self { args }
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(ViewArgs::from_input(ctx)?))
    }

    pub fn args(&self) -> &ViewArgs {
        &self.args
    }

    /// Open the view through the frontend's entry point.
    pub fn run(self, entry: &dyn ViewEntry) -> Result<i32, CommandError> {
        entry.open_view(self.args)
    }
}

/// Why `Dispatch` refuses to build `view`: it is a frontend entry point.
pub const NOT_DISPATCHED: &str =
    "`awman view` opens a TUI view of an engine instance and runs only from a shell";
