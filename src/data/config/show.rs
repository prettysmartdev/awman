//! Serializable config table rows and edit requests.

/// A config row's editing behaviour, derived from its key and handed to
/// frontends as data (the TUI's config dialog never inspects key names).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigRowKind {
    Scalar,
    /// The header of a user-named map (`agents.flags`); members are added as
    /// `<child_prefix><name>`.
    MapHeader {
        child_prefix: String,
    },
    ListHeader {
        child_prefix: String,
    },
    MapEntry,
    ListEntry,
}

impl ConfigRowKind {
    pub fn child_prefix(&self) -> Option<&str> {
        match self {
            Self::MapHeader { child_prefix } | Self::ListHeader { child_prefix } => {
                Some(child_prefix)
            }
            _ => None,
        }
    }

    pub fn is_header(&self) -> bool {
        self.child_prefix().is_some()
    }

    pub fn member_shape(&self) -> Option<ConfigRowKind> {
        match self {
            Self::MapHeader { .. } => Some(Self::MapEntry),
            Self::ListHeader { .. } => Some(Self::ListEntry),
            _ => None,
        }
    }
}

/// A user edit returned from the config show dialog.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConfigEditRequest {
    pub field: String,
    pub value: String,
    pub global: bool,
}

impl ConfigEditRequest {
    pub fn to_field(field: impl Into<String>, value: impl Into<String>, global: bool) -> Self {
        Self {
            field: field.into(),
            value: value.into(),
            global,
        }
    }

    /// A new member, named `member`, of the map whose header has `shape`.
    pub fn new_member(
        shape: &ConfigRowKind,
        member: &str,
        value: impl Into<String>,
        global: bool,
    ) -> Option<Self> {
        let prefix = shape.child_prefix()?;
        Some(Self {
            field: format!("{prefix}{member}"),
            value: value.into(),
            global,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConfigShowRejectedEdit {
    pub field: String,
    pub value: String,
    pub global: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConfigShowRow {
    pub field: String,
    pub global: String,
    pub repo: String,
    pub effective: String,
    pub read_only: bool,
    /// Whether the value may be written to the global config scope.
    pub global_writable: bool,
    /// Whether the value may be written to the repo config scope.
    pub repo_writable: bool,
    /// Short format hint shown while editing (e.g. "true or false").
    pub value_hint: Option<String>,
    /// The row's shape, carried through from `ConfigFieldRow` so the dialog
    /// can find the map and list headers — and build a new member's field
    /// name from the header's own prefix — without spelling a config path
    /// (WI 0114 F-20).
    pub shape: ConfigRowKind,
}
