//! `Prompt<D>` — a question, its answers, and what a dismissal means.
//!
//! Every interactive `ask_*` across awman is the same shape: a title, some
//! body text, a set of choices each with a hotkey and a label, and a decision
//! to fall back on when the user dismisses the dialog. Before WI 0114 F-19
//! each frontend composed those strings itself, so the CLI, the TUI and the
//! API asked the same question in three different wordings — and a change to
//! one was a change to one.
//!
//! **Layer 0 holds the shape; Layer 2 holds the copy.** The type is plain
//! data with no behaviour, so a Layer 1 trait can name it
//! (`InitFrontend::ask_dockerfile_setup` and friends cannot name a Layer 2
//! type). The constructors that fill in titles, labels and hotkeys live in
//! `command::prompts`, and Layer 2 hands a built `Prompt` down to Layer 1
//! through the engines' option structs, so no engine authors copy either.
//!
//! `D` is the decision type the command already had — `MountScopeDecision`,
//! `WorktreeMergeMode`, and so on. A frontend renders the choices, maps a
//! keypress or an index back to the chosen `D`, and returns it; it never
//! constructs a `D` from a string.

/// One selectable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice<D> {
    /// The key a terminal frontend accepts for this choice. Unique within a
    /// prompt — [`Prompt::new`] is where that is checked.
    pub key: char,
    /// What the user reads. Already complete: a frontend adds decoration
    /// (`[r]`, `▸`) but never words.
    pub label: String,
    /// What choosing this answers.
    pub value: D,
}

impl<D> Choice<D> {
    pub fn new(key: char, label: impl Into<String>, value: D) -> Self {
        Self {
            key,
            label: label.into(),
            value,
        }
    }
}

/// A question with a fixed set of answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt<D> {
    /// One line, shown as the dialog's title or the first line of a CLI
    /// prompt.
    pub title: String,
    /// Optional detail below the title. Empty when the title says it all.
    pub body: String,
    pub choices: Vec<Choice<D>>,
    /// What an Esc, a Ctrl-C or a closed stdin means.
    ///
    /// `None` makes dismissal an abort: the frontend returns
    /// `CommandError::Aborted` and the command stops. `Some(d)` is for the
    /// questions where refusing to choose *is* an answer.
    ///
    /// Every `Some` in the tree, and why — the list the work item asks this
    /// constructor to carry, so a new one is added deliberately:
    ///
    /// | Prompt | Answer | Why |
    /// |---|---|---|
    /// | `prompts::squad_task_confirm` | `Dismiss` | Walking away from a confirmation is a refusal; Esc must not trigger, cancel or pause anything. |
    /// | `prompts::agents_kill_confirm` | `Keep` | Walking away from a confirmation is a refusal; Esc must not stop an agent. |
    /// | `prompts::agent_trust_confirm` | `Decline` | Walking away from an approval is a refusal; a dismissal must never trust a definition. |
    /// | `prompts::dockerfile_setup` | `CreateNew` | The only choice that leaves `init` able to finish, it matches every headless profile, and it writes a file the user can replace or delete. |
    ///
    /// | `prompts::prerequisite_identity`, `_identity_persist` | `Continue`, `RunOnly` | Dismissal grants or persists nothing. |
    /// | `prompts::prerequisite_ssh`, `_token_consent`, `_token_source`, `_acquire_approval` | Decline | Credentials and host commands require explicit consent. |
    /// | `prompts::prerequisite_missing_tool`, `_gap`, `_report` | Continue | Esc leaves the run decision to the unresolved policy; summary Ctrl-C explicitly aborts. |
    ///
    /// Interview prompts are `None`: `work_item_kind`, `squad_task_workspace`
    /// and `squad_task_mount_scope` are interview steps whose answers are
    /// captured once and bind everything afterwards, so a dismissal must not
    /// answer them.
    pub default_on_dismiss: Option<D>,
}

impl<D> Prompt<D> {
    /// Build a prompt.
    ///
    /// # Panics
    ///
    /// If two choices share a hotkey, or there are no choices. Both are
    /// programming errors in a `const`-shaped constructor, not user input:
    /// a duplicate hotkey makes one answer unreachable, which a test would
    /// only catch if it happened to press that key.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        choices: Vec<Choice<D>>,
        default_on_dismiss: Option<D>,
    ) -> Self {
        assert!(
            !choices.is_empty(),
            "a prompt must offer at least one answer"
        );
        for (i, choice) in choices.iter().enumerate() {
            assert!(
                !choices[..i].iter().any(|c| c.key == choice.key),
                "duplicate hotkey '{}' in prompt",
                choice.key
            );
        }
        Self {
            title: title.into(),
            body: body.into(),
            choices,
            default_on_dismiss,
        }
    }

    /// The choice bound to `key`, if any. Case-sensitive: a prompt that wants
    /// `y` and `Y` to differ can have both.
    pub fn choice_for_key(&self, key: char) -> Option<&Choice<D>> {
        self.choices.iter().find(|c| c.key == key)
    }

    /// The choice at `index`, for a list-picker frontend.
    pub fn choice_at(&self, index: usize) -> Option<&Choice<D>> {
        self.choices.get(index)
    }

    /// The hotkeys, in order — `[y, n]`, `[r, c, a]`.
    pub fn keys(&self) -> Vec<char> {
        self.choices.iter().map(|c| c.key).collect()
    }

    /// The labels, in order.
    pub fn labels(&self) -> Vec<&str> {
        self.choices.iter().map(|c| c.label.as_str()).collect()
    }
}

impl<D: Clone> Prompt<D> {
    /// The answer for `key`, cloned.
    pub fn answer_for_key(&self, key: char) -> Option<D> {
        self.choice_for_key(key).map(|c| c.value.clone())
    }

    /// The answer at `index`, cloned.
    pub fn answer_at(&self, index: usize) -> Option<D> {
        self.choice_at(index).map(|c| c.value.clone())
    }
}

/// A question whose answer is free text.
///
/// The sibling of [`Prompt`] for the interview steps that take a name, a
/// path or an interval rather than a choice. `default` is what an empty
/// submission means — and it is *the* place that default is written, which is
/// the point: `"6h"` was spelled in the catalogue, in the CLI frontend and in
/// the TUI frontend, and only the catalogue's copy was ever tested
/// (WI 0114 F-19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPrompt {
    /// One line, shown as the dialog's title.
    pub title: String,
    /// The question itself, shown above the input.
    pub body: String,
    /// What an empty submission means. `None` makes an empty submission an
    /// error rather than an answer.
    pub default: Option<String>,
    /// The frontend conceals typed characters and does not retain input history.
    pub masked: bool,
}

impl TextPrompt {
    pub fn masked(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            default: None,
            masked: true,
        }
    }

    pub fn new(title: impl Into<String>, body: impl Into<String>, default: Option<String>) -> Self {
        Self {
            masked: false,
            title: title.into(),
            body: body.into(),
            default,
        }
    }

    /// Resolve what the user typed: the trimmed text, or the default when it
    /// is empty. `None` when the text is empty and there is no default.
    pub fn resolve(&self, typed: &str) -> Option<String> {
        let trimmed = typed.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        self.default.clone()
    }
}
