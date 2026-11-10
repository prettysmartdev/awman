//! Serializable dialog requests and answers; rendering state lives in the frontend.
use crate::data::config::show::{ConfigEditRequest, ConfigShowRejectedEdit, ConfigShowRow};
pub use crate::data::ids::DialogId;
use crate::data::prompt::Prompt;

/// A dialog request sent from the command thread to the event loop.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DialogRequest {
    YesNo {
        title: String,
        body: String,
    },
    YesNoCancel {
        title: String,
        body: String,
    },
    TextInput {
        title: String,
        prompt: String,
        default_text: Option<String>,
    },
    /// A masked prompt for a secret (WI 0149). Typed characters render as
    /// `•`; the value is never echoed, logged or kept after the answer.
    SecretInput {
        title: String,
        prompt: String,
    },
    /// Leave the alternate screen and raw mode so a host command can use the
    /// terminal. Answered `Yes` once the terminal is released.
    TerminalRelease {
        title: String,
        terminal_id: Option<crate::data::ids::TerminalId>,
    },
    /// Re-enter the alternate screen after a `TerminalRelease`. Answered
    /// `Dismissed` once the screen is back.
    TerminalRestore,
    MultilineInput {
        title: String,
        prompt: String,
        /// Text the editor opens holding (WI 0110). `None` opens empty. Used
        /// by edit interviews, where an untouched box must mean "keep what
        /// is there" rather than "erase it".
        default_text: Option<String>,
    },
    ListPicker {
        title: String,
        items: Vec<String>,
    },
    KindSelect {
        title: String,
        options: Vec<(String, String)>,
    },
    WorkflowControlBoard(WorkflowControlBoardState),
    WorkflowYoloCountdown(WorkflowYoloCountdownState),
    WorkflowStepConfirm(WorkflowStepConfirmState),
    AgentSetup(AgentSetupState),
    MountScope(MountScopeState),
    AgentAuth(AgentAuthState),
    QuitConfirm,
    CloseTabConfirm,
    /// Confirmation prompt opened when the user presses Ctrl+C while a
    /// workflow is running. `y` aborts the workflow (kills the container,
    /// returns the current step to Pending), `n`/`Esc` keeps it running.
    WorkflowCancelConfirm,
    ConfigShow {
        rows: Vec<ConfigShowRow>,
        /// Initially selected row, so the dialog can reopen on the field the
        /// user just edited instead of jumping back to the top.
        selected: usize,
        /// The previous edit, when it was rejected (invalid value or failed
        /// write). The dialog reopens in edit mode with the input preserved
        /// and the reason displayed.
        rejected: Option<ConfigShowRejectedEdit>,
    },
    Loading {
        title: String,
    },
    Custom {
        title: String,
        body: String,
        keys: Vec<(char, String)>,
    },
    /// A keyed question whose body scrolls under a footer that is always on
    /// screen (the note, the keys and the Esc hint). Used for the trust
    /// reviews, where a `gated_keys` choice is accepted only once the body
    /// has been read in full, and for every question whose body can outgrow
    /// the terminal (workflow prerequisites, `config migrate`).
    Review(ReviewRequest),
    /// The one-shot squad key disclosure, raised as a [`Dialog::Notice`].
    /// Sent, never awaited: a notice has no answer to give back, and the
    /// command thread must not block on the user dismissing it.
    KeySetupNotice {
        title: String,
        body: String,
        copy_key: String,
        copy_zshrc_snippet: String,
    },
}

/// A dialog response returned from the event loop to the command thread.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DialogResponse {
    Interrupted,
    Yes,
    No,
    Cancel,
    Text(String),
    /// The answer to a `SecretInput`. Wrapped in a `SecretString` before it
    /// leaves the dialog; its `Debug` is redacted.
    #[serde(with = "crate::data::secret::frontend_wire")]
    Secret(crate::data::secret::SecretString),
    Index(usize),
    Char(char),
    Dismissed,
    /// A config-table edit, as the command layer's own type.
    ///
    /// Before WI 0114 F-20 this rode `Text` as a tab-separated
    /// `"field\tvalue\tscope"` string that `dialog_router.rs` formatted and
    /// `per_command/config.rs` split back apart — a private wire protocol
    /// between two files, with a silent `Ok(None)` whenever the split did not
    /// yield three parts.
    ConfigEdit(ConfigEditRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowControlBoardState {
    pub step_name: String,
    pub can_launch_next: bool,
    pub can_continue_current: bool,
    pub can_restart: bool,
    pub can_go_back: bool,
    pub can_finish: bool,
    /// Human-readable reason explaining why "continue in current container"
    /// is not available (e.g. "next step uses a different agent"). Rendered
    /// in DarkGray underneath the disabled `[↓]` line so users understand
    /// why it's greyed out.
    pub continue_unavailable_reason: Option<String>,
    pub cancel_to_previous_unavailable_reason: Option<String>,
    pub finish_workflow_unavailable_reason: Option<String>,
    /// Reason "Restart current step" is unavailable — set in a parallel group
    /// where restart applies only to the focused container (WI-0096 §10).
    /// Rendered in DarkGray under the dimmed Restart line.
    pub restart_unavailable_reason: Option<String>,
    /// True when a container is currently running (mid-step). The engine
    /// computes this from `can_dismiss` in `AvailableActions`.
    /// Changes rendering: Esc = dismiss (step keeps running), [p] = pause.
    pub can_dismiss: bool,
    /// Custom label for the right-arrow (launch-next) action, copied from
    /// `AvailableActions.launch_next_label`. `None` renders the default
    /// "Next: new container". The dynamic leader step sets this to
    /// "Start dynamic workflow".
    pub launch_next_label: Option<String>,
    /// The step this Workflow Control Board's actions apply to (WI-0096 §10).
    /// In a parallel group this is the currently-focused container. Defaults to
    /// `step_name` for single-step workflows.
    pub focused_step_name: String,
    /// Number of steps in the focused step's parallel group. `0` means the
    /// focused step is not part of a multi-step parallel batch.
    pub parallel_peer_count: usize,
    /// Live peers still running in the focused step's parallel group (excludes
    /// the focused step). A final yolo review can finish with peers still live.
    pub parallel_peers_running: usize,
    /// Detail lines for the failure that opened this board (exit code, signal,
    /// run duration), copied from `AvailableActions::step_failure`. Empty on an
    /// ordinary between-steps board (WI-0115 §1).
    pub failure_lines: Vec<String>,
    /// A peer in the still-running parallel group that already failed and
    /// can be relaunched now with `[r]`, copied from
    /// `AvailableActions::retry_failed_step`. Rendered as a red line below the
    /// arrow options.
    pub retry_failed_step: Option<String>,
    /// True when the board was opened while a parallel group is running. The
    /// arrows then act on the whole group, and their labels say so.
    pub in_parallel_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowYoloCountdownState {
    pub step_name: String,
    pub remaining_secs: u64,
    /// Why the countdown runs; selects the wording.
    #[serde(rename = "countdown_kind")]
    pub kind: crate::data::workflow_state::CountdownKind,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowStepConfirmState {
    pub completed_step: String,
    pub next_step: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentSetupState {
    pub agent_name: String,
    pub image_only: bool,
    pub has_fallback: bool,
    pub fallback_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountScopeState {
    pub git_root: String,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentAuthState {
    pub agent_name: String,
    pub env_vars: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ReviewBody {
    /// Every character kept, hard-wrapped at the edge: a reviewed argv must
    /// read exactly as it will run.
    Exact(String),
    /// Word-wrapped text; each line keeps its own indentation, so indented
    /// JSON in a preview still lines up.
    Prose(String),
    /// The workflow prerequisite report, one entry per check.
    Prerequisites(crate::data::workflow_prerequisites::WorkflowPrerequisiteReport),
}

impl DialogRequest {
    pub fn awaits_answer(&self) -> bool {
        !matches!(self, Self::KeySetupNotice { .. } | Self::Loading { .. })
    }
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::YesNo { .. } => "yes_no",
            Self::YesNoCancel { .. } => "yes_no_cancel",
            Self::TextInput { .. } => "text_input",
            Self::SecretInput { .. } => "secret_input",
            Self::TerminalRelease { .. } => "terminal_release",
            Self::TerminalRestore => "terminal_restore",
            Self::MultilineInput { .. } => "multiline_input",
            Self::ListPicker { .. } => "list_picker",
            Self::KindSelect { .. } => "kind_select",
            Self::WorkflowControlBoard(_) => "workflow_control_board",
            Self::WorkflowYoloCountdown(_) => "workflow_yolo_countdown",
            Self::WorkflowStepConfirm(_) => "workflow_step_confirm",
            Self::AgentSetup(_) => "agent_setup",
            Self::MountScope(_) => "mount_scope",
            Self::AgentAuth(_) => "agent_auth",
            Self::QuitConfirm => "quit_confirm",
            Self::CloseTabConfirm => "close_tab_confirm",
            Self::WorkflowCancelConfirm => "workflow_cancel_confirm",
            Self::ConfigShow { .. } => "config_show",
            Self::Loading { .. } => "loading",
            Self::Custom { .. } => "custom",
            Self::Review(_) => "review",
            Self::KeySetupNotice { .. } => "key_setup_notice",
        }
    }
    /// Validates the answer shape and the set of choices offered by this request.
    pub fn accepts(&self, response: &DialogResponse) -> bool {
        use DialogResponse as R;
        if !self.awaits_answer() {
            return false;
        }
        if matches!(response, R::Interrupted) {
            return true;
        }
        match self {
            Self::YesNo { .. } => {
                matches!(response, R::Yes | R::No | R::Cancel | R::Dismissed)
            }
            Self::YesNoCancel { .. } => {
                matches!(response, R::Yes | R::No | R::Cancel | R::Dismissed)
            }
            Self::TextInput { .. } | Self::MultilineInput { .. } => {
                matches!(response, R::Text(_) | R::Cancel | R::Dismissed)
            }
            Self::SecretInput { .. } => matches!(response, R::Secret(_) | R::Cancel | R::Dismissed),
            Self::TerminalRelease { .. } => matches!(response, R::Yes),
            Self::TerminalRestore => matches!(response, R::Dismissed),
            Self::ListPicker { items, .. } => {
                matches!(response,R::Index(index) if *index < items.len())
                    || matches!(response, R::Cancel | R::Dismissed)
            }
            Self::KindSelect { options, .. } => {
                matches!(response,R::Index(index) if *index < options.len())
                    || matches!(response, R::Cancel | R::Dismissed)
            }
            Self::ConfigShow { .. } => {
                matches!(response, R::ConfigEdit(_) | R::Cancel | R::Dismissed)
            }
            Self::Custom { keys, .. } => {
                matches!(response,R::Char(key) if keys.iter().any(|(k,_)| k == key))
                    || matches!(response, R::Cancel | R::Dismissed)
            }
            Self::Review(review) => {
                matches!(response,R::Char(key) if review.keys.iter().any(|(k,_)| k == key))
                    || matches!(response, R::Cancel | R::Dismissed)
            }
            Self::QuitConfirm | Self::CloseTabConfirm => {
                matches!(response, R::Yes | R::No | R::Cancel | R::Dismissed)
            }
            Self::MountScope(_)
            | Self::WorkflowCancelConfirm
            | Self::WorkflowControlBoard(_)
            | Self::WorkflowYoloCountdown(_)
            | Self::WorkflowStepConfirm(_)
            | Self::AgentSetup(_)
            | Self::AgentAuth(_) => matches!(response, R::Char(_) | R::Cancel | R::Dismissed),
            Self::Loading { .. } | Self::KeySetupNotice { .. } => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewRequest {
    pub title: String,
    pub body: ReviewBody,
    pub note: String,
    pub keys: Vec<(char, String)>,
    pub gated_keys: Vec<char>,
    pub dismiss_label: Option<String>,
}
impl ReviewRequest {
    pub fn new(
        title: String,
        body: String,
        keys: Vec<(char, String)>,
        gated_keys: Vec<char>,
    ) -> Self {
        Self {
            title,
            body: ReviewBody::Exact(body),
            note: String::new(),
            keys,
            gated_keys,
            dismiss_label: None,
        }
    }

    /// A question with no gated key: `body` scrolls, Esc answers
    /// `default_on_dismiss` and is labelled with that choice.
    pub fn question<D: PartialEq>(prompt: &Prompt<D>, body: ReviewBody) -> Self {
        let dismiss_label = prompt.default_on_dismiss.as_ref().and_then(|default| {
            prompt
                .choices
                .iter()
                .find(|choice| &choice.value == default)
                .map(|choice| choice.label.clone())
        });
        Self {
            dismiss_label,
            ..Self::new(
                prompt.title.clone(),
                String::new(),
                prompt
                    .choices
                    .iter()
                    .map(|choice| (choice.key, choice.label.clone()))
                    .collect(),
                Vec::new(),
            )
        }
        .with_body(body)
    }

    pub fn with_body(mut self, body: ReviewBody) -> Self {
        self.body = body;
        self
    }

    pub fn with_note(mut self, note: String) -> Self {
        self.note = note;
        self
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::data::secret::SecretString;
    use serde_json::json;
    pub(crate) fn requests(text: &str) -> Vec<DialogRequest> {
        let mut requests = vec![
            json!({"kind":"yes_no","title":text,"body":text}),
            json!({"kind":"yes_no_cancel","title":text,"body":text}),
            json!({"kind":"text_input","title":text,"prompt":text,"default_text":text}),
            json!({"kind":"secret_input","title":text,"prompt":text}),
            json!({"kind":"terminal_release","title":text,"terminal_id":crate::data::ids::TerminalId::new()}),
            json!({"kind":"terminal_restore"}),
            json!({"kind":"multiline_input","title":text,"prompt":text,"default_text":null}),
            json!({"kind":"list_picker","title":text,"items":[text]}),
            json!({"kind":"kind_select","title":text,"options":[[text,text]]}),
            json!({"kind":"workflow_control_board","step_name":text,"can_launch_next":true,"can_continue_current":false,"can_restart":true,"can_go_back":false,"can_finish":true,"can_dismiss":true,"focused_step_name":text,"parallel_peer_count":2,"parallel_peers_running":1,"failure_lines":[text],"in_parallel_group":true}),
            json!({"kind":"workflow_yolo_countdown","step_name":text,"remaining_secs":42,"countdown_kind":"stuck_step"}),
            json!({"kind":"workflow_step_confirm","completed_step":text,"next_step":text}),
            json!({"kind":"agent_setup","agent_name":text,"image_only":false,"has_fallback":true,"fallback_name":text}),
            json!({"kind":"mount_scope","git_root":text,"cwd":text}),
            json!({"kind":"agent_auth","agent_name":text,"env_vars":[text]}),
            json!({"kind":"quit_confirm"}), json!({"kind":"close_tab_confirm"}), json!({"kind":"workflow_cancel_confirm"}),
            json!({"kind":"config_show","rows":[{"field":text,"global":text,"repo":text,"effective":text,"read_only":false,"global_writable":true,"repo_writable":true,"value_hint":text,"shape":{"map_header":{"child_prefix":text}}}],"selected":0,"rejected":{"field":text,"value":text,"global":true,"reason":text}}),
            json!({"kind":"loading","title":text}),
            json!({"kind":"custom","title":text,"body":text,"keys":[["a",text]]}),
            json!({"kind":"key_setup_notice","title":text,"body":text,"copy_key":text,"copy_zshrc_snippet":text}),
        ].into_iter().map(|value| {
            serde_json::from_value(value).unwrap()
        }).collect::<Vec<_>>();
        for body in [
            ReviewBody::Exact(text.into()),
            ReviewBody::Prose(text.into()),
            ReviewBody::Prerequisites(
                crate::data::workflow_prerequisites::WorkflowPrerequisiteReport {
                    phase: crate::data::workflow_prerequisites::PrerequisitePhase::Start,
                    origin: crate::data::workflow_prerequisites::PrerequisiteOrigin::Static,
                    checks: vec![],
                    advisories: vec![],
                    notes: vec![text.into()],
                },
            ),
        ] {
            requests.push(DialogRequest::Review(
                ReviewRequest::new(
                    text.into(),
                    text.into(),
                    vec![('a', text.into())],
                    vec!['a'],
                )
                .with_body(body),
            ));
        }
        requests
    }
    pub(crate) fn responses(text: &str) -> Vec<DialogResponse> {
        vec![
            DialogResponse::Interrupted,
            DialogResponse::Yes,
            DialogResponse::No,
            DialogResponse::Cancel,
            DialogResponse::Text(text.into()),
            DialogResponse::Secret(SecretString::new(text)),
            DialogResponse::Index(usize::MAX),
            DialogResponse::Char('é'),
            DialogResponse::Dismissed,
            DialogResponse::ConfigEdit(ConfigEditRequest::to_field(text, text, true)),
        ]
    }
    #[test]
    fn generated_dialog_variants_round_trip_and_redact() {
        for seed in 0..128 {
            let text = format!("value-{seed}: {}", "é\n\t\"\\🙂".repeat(seed % 13));
            for request in requests(&text) {
                let encoded = serde_json::to_string(&request).unwrap();
                assert_eq!(
                    serde_json::from_str::<DialogRequest>(&encoded).unwrap(),
                    request
                );
                assert!(!request.kind_name().is_empty());
            }
            for response in responses(&text) {
                let encoded = serde_json::to_string(&response).unwrap();
                assert_eq!(
                    serde_json::from_str::<DialogResponse>(&encoded).unwrap(),
                    response
                );
            }
        }
        let debug = format!(
            "{:?}",
            DialogResponse::Secret(SecretString::new("DO_NOT_PRINT"))
        );
        assert!(!debug.contains("DO_NOT_PRINT"));
        assert!(debug.contains("<redacted>"));
    }
    #[test]
    fn answer_shapes_and_offered_choices_are_checked() {
        let list = DialogRequest::ListPicker {
            title: "".into(),
            items: vec!["one".into()],
        };
        assert!(list.accepts(&DialogResponse::Index(0)));
        assert!(!list.accepts(&DialogResponse::Index(1)));
        assert!(!list.accepts(&DialogResponse::Text("wrong".into())));
        let review = DialogRequest::Review(ReviewRequest::new(
            "".into(),
            "".into(),
            vec![('a', "approve".into())],
            vec!['a'],
        ));
        assert!(review.accepts(&DialogResponse::Char('a')));
        assert!(!review.accepts(&DialogResponse::Char('z')));
        assert!(!DialogRequest::Loading { title: "".into() }.awaits_answer());
        assert!(!DialogRequest::Loading { title: "".into() }.accepts(&DialogResponse::Interrupted));
    }
}
