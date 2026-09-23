//! Live-session credential tracking: the lease registry and the refresh
//! monitor (WI-0107 §3).
//!
//! A [`CredentialLease`] is the RAII proof that a credentialed container is
//! live; the [`CredentialRefreshMonitor`] owns the [`LeaseRegistry`] and keeps
//! every live container's staged credential file fresh without ever writing to
//! a dead session's path. See `lease.rs` and `monitor.rs` for the invariant
//! walkthroughs.

pub mod lease;
pub mod monitor;

pub use lease::{CredentialLease, LeaseGeneration, LeaseRegistry, LeaseSnapshot};
pub use monitor::{CredentialRefreshMonitor, MonitorConfig, RefreshOutcome, RefreshStatus};

use std::sync::Arc;

use crate::engine::auth::RefreshableCredentialDelivery;
use crate::engine::container::options::ResolvedContainerOptions;

/// Whatever can hand out a [`CredentialLease`] for a launch's file-delivered
/// credentials.
///
/// Before WI 0114 F-38 the container backends reached the monitor through a
/// process-global `OnceLock`, so whether leases were taken depended on hidden
/// state installed somewhere else in the process. The factory is now carried
/// explicitly on [`ResolvedContainerOptions`]: options built without one take
/// no leases, which is the same behaviour the absent global produced and is
/// also how the `authRefresh.enabled: false` kill switch is implemented.
pub trait CredentialLeaseFactory: Send + Sync {
    fn register_lease(
        &self,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
    ) -> CredentialLease;
}

/// The factory as it travels on a `ContainerOption` / `ResolvedContainerOptions`.
///
/// A newtype rather than a bare `Arc<dyn …>` so those two types keep their
/// derived `Debug` and `PartialEq`. Two handles are equal when they point at
/// the same factory, and the `Debug` says only that one is present — never
/// anything credential-derived (INV-5).
#[derive(Clone)]
pub struct LeaseFactoryHandle(Arc<dyn CredentialLeaseFactory>);

impl LeaseFactoryHandle {
    pub fn new(factory: Arc<dyn CredentialLeaseFactory>) -> Self {
        Self(factory)
    }

    fn register_lease(
        &self,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
    ) -> CredentialLease {
        self.0.register_lease(delivery, container)
    }
}

impl PartialEq for LeaseFactoryHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for LeaseFactoryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LeaseFactoryHandle(..)")
    }
}

/// The monitor registers through `Arc<Self>` (it hands a clone to the tick
/// thread), so the factory impl lives on the handle rather than on the value.
impl CredentialLeaseFactory for Arc<CredentialRefreshMonitor> {
    fn register_lease(
        &self,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
    ) -> CredentialLease {
        CredentialRefreshMonitor::register(self, delivery, container)
    }
}

/// Take a [`CredentialLease`] for every file-delivered credential this launch
/// carries. Called from the container backends' `build()` — the single spawn
/// choke point — so no per-command frontend ever registers a lease.
///
/// Returns an empty vec when the options carry no lease factory (no monitor,
/// or the `authRefresh.enabled: false` kill switch) or when the launch carries
/// no file-delivered credential — which is every launch today. A non-empty
/// [`ResolvedContainerOptions::refreshable_credentials`] together with a
/// factory MUST yield a non-empty lease vec before the child is spawned
/// (INV-6); the backends assert this.
pub(crate) fn register_container_leases(
    options: &ResolvedContainerOptions,
    container: &str,
) -> Vec<CredentialLease> {
    if options.refreshable_credentials.is_empty() {
        return Vec::new();
    }
    match options.lease_factory.as_ref() {
        Some(factory) => options
            .refreshable_credentials
            .iter()
            .map(|delivery| factory.register_lease(delivery, container))
            .collect(),
        None => Vec::new(),
    }
}
