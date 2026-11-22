//! The integration registry (WI 0142 Part A §4): the compiled-in list of
//! integration descriptors (`name`, `detect(env, config) -> HostRequirement`,
//! optional `setup`/`remove`/`status`, optional `HostPresenter`) and the source
//! of hand-off policy. Rex (WI 0157) registers here; no integration-specific
//! type appears in the protocol, the store or `src/data`.
//!
//! Layer 2. Presenters are Layer 3 adapters that implement the
//! [`presenter::HostPresenter`] trait defined here and are supervised
//! separately from the Layer 1 feature.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.9.

pub mod presenter;

use std::sync::Arc;

use crate::command::error::CommandError;
use crate::data::annotations::{AnnotationError, Annotations};
use crate::data::config::effective::EffectiveConfig;
use crate::data::config::tui::IntegrationSwitch;
use crate::data::config::EnvSnapshot;
use crate::data::frontend_protocol::{UiOutcome, UiRequest};
use crate::data::ids::{FrontendId, InstanceId};
use crate::data::message::MessageLevel;
use crate::data::session::Session;

pub use presenter::{
    AnnotationWriter, HostPresenter, NoticeSink, PresenterEvent, PresenterHandle,
    PresenterSupervisor,
};

/// What an integration needs from the host it detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRequirement {
    None,
    Optional(Annotations),
    Required(Annotations),
}

impl HostRequirement {
    pub fn annotations(&self) -> Option<&Annotations> {
        match self {
            Self::None => None,
            Self::Optional(a) | Self::Required(a) => Some(a),
        }
    }
    pub fn requires_daemon(&self) -> bool {
        matches!(self, Self::Required(_))
    }
    fn strength(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Optional(_) => 1,
            Self::Required(_) => 2,
        }
    }
}

pub struct IntegrationContext<'a> {
    pub env: &'a EnvSnapshot,
    pub config: &'a EffectiveConfig,
    pub session: Option<&'a Session>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrationReport {
    pub lines: Vec<String>,
    pub ok: bool,
}

/// `CommandError` has no `IntegrationUnsupported` variant yet (it belongs to
/// the error-surface step); this is the one place to swap when it lands.
fn unsupported(name: &str, op: &str) -> CommandError {
    CommandError::Other(format!("integration `{name}` does not support `{op}`"))
}

/// A compiled-in integration descriptor.
pub trait Integration: Send + Sync + 'static {
    /// Annotation namespace and `tui.integrations.<name>` config key.
    fn name(&self) -> &'static str;
    /// Must honour `IntegrationSwitch::Off` by returning `None`
    /// (see [`switch_is_off`]).
    fn detect(&self, env: &EnvSnapshot, config: &EffectiveConfig) -> HostRequirement;
    fn setup(&self, _ctx: &IntegrationContext<'_>) -> Result<IntegrationReport, CommandError> {
        Err(unsupported(self.name(), "setup"))
    }
    fn remove(&self, _ctx: &IntegrationContext<'_>) -> Result<IntegrationReport, CommandError> {
        Err(unsupported(self.name(), "remove"))
    }
    fn status(&self, _ctx: &IntegrationContext<'_>) -> Result<IntegrationReport, CommandError> {
        Err(unsupported(self.name(), "status"))
    }
    fn presenter(&self) -> Option<Arc<dyn HostPresenter>> {
        None
    }
}

/// Helper for `detect` implementations: is this integration switched off?
pub fn switch_is_off(name: &str, config: &EffectiveConfig) -> bool {
    config.integration_switch(name) == IntegrationSwitch::Off
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The strongest requirement across enabled integrations.
    pub requirement: HostRequirement,
    /// Merged annotations of every integration that detected something.
    pub annotations: Annotations,
    pub required_by: Vec<&'static str>,
}

pub struct IntegrationRegistry {
    items: Vec<Arc<dyn Integration>>,
}

impl IntegrationRegistry {
    /// Empty in 0142; 0157 adds Rex here.
    pub fn compiled_in() -> Arc<Self> {
        Self::with(Vec::new())
    }

    pub fn with(items: Vec<Arc<dyn Integration>>) -> Arc<Self> {
        Arc::new(Self { items })
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.items.iter().map(|i| i.name()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Integration>> {
        self.items.iter().find(|i| i.name() == name).cloned()
    }

    /// Run every descriptor's `detect` and merge the results. A key outside the
    /// detecting integration's namespace, or two integrations claiming the same
    /// key, is an error.
    pub fn detect_all(
        &self,
        env: &EnvSnapshot,
        config: &EffectiveConfig,
    ) -> Result<Detected, AnnotationError> {
        let mut merged = Annotations::new();
        let mut owners: Vec<&'static str> = Vec::new();
        let mut strongest = HostRequirement::None;
        let mut required_by = Vec::new();
        for item in &self.items {
            let name = item.name();
            let req = item.detect(env, config);
            let Some(ann) = req.annotations() else {
                continue;
            };
            if let Some(key) = ann
                .iter()
                .map(|(k, _)| k)
                .find(|k| Annotations::namespace_of(k) != Some(name))
            {
                return Err(AnnotationError::ForeignNamespace {
                    key: key.into(),
                    namespace: name.into(),
                });
            }
            let first = owners.last().copied().unwrap_or(name);
            merged.merge_disjoint(ann.clone(), name, first)?;
            owners.push(name);
            if matches!(req, HostRequirement::Required(_)) {
                required_by.push(name);
            }
            if req.strength() > strongest.strength() {
                strongest = req;
            }
        }
        // The strongest requirement carries the merged annotations.
        let requirement = match strongest {
            HostRequirement::None => HostRequirement::None,
            HostRequirement::Optional(_) => HostRequirement::Optional(merged.clone()),
            HostRequirement::Required(_) => HostRequirement::Required(merged.clone()),
        };
        Ok(Detected {
            requirement,
            annotations: merged,
            required_by,
        })
    }

    /// A presenter claims every frontend with a key in its namespace.
    pub fn presenter_for(&self, annotations: &Annotations) -> Option<Arc<dyn HostPresenter>> {
        self.items
            .iter()
            .filter(|i| annotations.has_namespace(i.name()))
            .find_map(|i| i.presenter())
    }

    pub fn presenters(&self) -> Vec<Arc<dyn HostPresenter>> {
        self.items.iter().filter_map(|i| i.presenter()).collect()
    }

    /// Route one UI request by the frontend's `Hello` annotations. No claiming
    /// presenter, or a presenter that panics → `claimed_by_host: false` and
    /// one notice through `notices`.
    pub fn route_ui_request(
        &self,
        hello: &Annotations,
        instance: &InstanceId,
        frontend: &FrontendId,
        req: &UiRequest,
        notices: &NoticeSink,
    ) -> UiOutcome {
        match self.presenter_for(hello) {
            None => {
                notices(
                    MessageLevel::Info,
                    "no integration host claimed this frontend; the request was handled by the engine".into(),
                );
                UiOutcome::unclaimed()
            }
            Some(p) => presenter::call_ui_request(p.as_ref(), instance, frontend, req, notices),
        }
    }
}
