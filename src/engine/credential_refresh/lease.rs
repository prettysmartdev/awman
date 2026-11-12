//! Lease registry — the RAII proof that a credentialed container is live.
//!
//! A [`CredentialLease`] is handed out by [`LeaseRegistry::register`] at the
//! single container-spawn choke point and is owned by the container's execution
//! backend, so it deregisters in [`Drop`] on **every** exit path — normal exit,
//! spawn failure, error propagation, panic unwind and Ctrl-C teardown alike
//! (INV-6). The monitor reads a point-in-time [`LeaseSnapshot`] list each tick
//! and re-checks [`LeaseRegistry::is_live`] before every write, so a lease
//! dropped mid-tick loses the race safely (INV-7).
//!
//! [`LeaseGeneration`] is monotonic and never reused: a recycled staged path
//! necessarily carries a *different* generation, which is the second of the
//! three independent defenses against writing into a dead session's directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use crate::data::session::AgentName;
use crate::engine::auth::RefreshableCredentialDelivery;

/// Monotonic id, unique per registration for the process's lifetime. Reused
/// paths get a NEW generation, so a stale tick can never write into a recycled
/// path (INV-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseGeneration(u64);

impl LeaseGeneration {
    /// The underlying counter value. For logging/tests only.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Registry-side record of one live lease. Carries no secret.
#[derive(Debug, Clone)]
struct LeaseRecord {
    agent: AgentName,
    spec_agent: &'static str,
    staged_path: PathBuf,
    staged_root: PathBuf,
    container: String,
}

#[derive(Default)]
struct RegistryInner {
    entries: HashMap<LeaseGeneration, LeaseRecord>,
    next_gen: u64,
    shutdown: bool,
}

/// Shared state behind the registry. The `Arc` is held by the [`LeaseRegistry`]
/// and (for parking/shutdown) by the monitor's background thread; each
/// [`CredentialLease`] holds only a `Weak` so a live lease never keeps the
/// registry alive on its own.
pub(super) struct RegistryShared {
    inner: Mutex<RegistryInner>,
    /// Signalled on empty→non-empty transitions and on shutdown so the monitor
    /// loop can park at zero CPU while no credentialed container is live.
    wake: Condvar,
}

impl RegistryShared {
    fn deregister(&self, generation: LeaseGeneration) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.remove(&generation);
        // No wake needed on empty: the monitor loop parks itself the next time
        // it observes an empty registry.
    }

    /// Block until at least one lease is registered or shutdown is requested.
    /// Returns `true` while there is work to do, `false` once shutdown.
    pub(super) fn wait_until_active(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        while inner.entries.is_empty() && !inner.shutdown {
            inner = self.wake.wait(inner).unwrap();
        }
        !inner.shutdown
    }

    /// Sleep up to `dur`, returning early if shutdown is requested. Returns
    /// `true` when shutdown was requested.
    pub(super) fn sleep_or_shutdown(&self, dur: Duration) -> bool {
        let inner = self.inner.lock().unwrap();
        let (inner, _timed_out) = self
            .wake
            .wait_timeout_while(inner, dur, |st| !st.shutdown)
            .unwrap();
        inner.shutdown
    }

    pub(super) fn request_shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutdown = true;
        self.wake.notify_all();
    }
}

/// RAII guard proving a credentialed container is live. Deregisters in [`Drop`].
///
/// It is MOVED into the container's execution backend by every spawn function,
/// because the instance box is dropped before the spawn function returns; the
/// backend is consumed when the child exits, so the lease's lifetime brackets
/// the child process exactly. Deliberately NOT `Clone`.
pub struct CredentialLease {
    registry: Weak<RegistryShared>,
    generation: LeaseGeneration,
    agent: AgentName,
    staged_path: PathBuf,
    container: String,
}

impl CredentialLease {
    pub fn generation(&self) -> LeaseGeneration {
        self.generation
    }

    pub fn agent(&self) -> &AgentName {
        &self.agent
    }

    /// Absolute staged credential-file path this lease covers.
    pub fn staged_path(&self) -> &Path {
        &self.staged_path
    }

    /// Container name, for logging only.
    pub fn container(&self) -> &str {
        &self.container
    }
}

impl Drop for CredentialLease {
    fn drop(&mut self) {
        if let Some(shared) = self.registry.upgrade() {
            shared.deregister(self.generation);
        }
    }
}

impl std::fmt::Debug for CredentialLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialLease")
            .field("generation", &self.generation)
            .field("agent", &self.agent)
            .field("container", &self.container)
            .field("staged_path", &self.staged_path)
            .finish()
    }
}

/// A tick-time copy of one live lease. Carries no secret.
#[derive(Debug, Clone)]
pub struct LeaseSnapshot {
    pub generation: LeaseGeneration,
    pub agent: AgentName,
    pub spec_agent: &'static str,
    pub staged_path: PathBuf,
    pub staged_root: PathBuf,
    pub container: String,
}

/// The process-wide set of live credential leases.
pub struct LeaseRegistry {
    shared: Arc<RegistryShared>,
}

impl Default for LeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LeaseRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseRegistry")
            .field("live", &self.len())
            .finish()
    }
}

impl LeaseRegistry {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(RegistryShared {
                inner: Mutex::new(RegistryInner::default()),
                wake: Condvar::new(),
            }),
        }
    }

    /// Register a live credentialed container and hand back the RAII guard.
    /// Called ONLY from the container backends' `build()` via the monitor.
    pub fn register(
        &self,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
    ) -> CredentialLease {
        let mut inner = self.shared.inner.lock().unwrap();
        let was_empty = inner.entries.is_empty();
        let generation = LeaseGeneration(inner.next_gen);
        inner.next_gen += 1;
        inner.entries.insert(
            generation,
            LeaseRecord {
                agent: delivery.agent.clone(),
                spec_agent: delivery.spec_agent,
                staged_path: delivery.staged_path.clone(),
                staged_root: delivery.staged_root.clone(),
                container: container.to_string(),
            },
        );
        if was_empty {
            // Wake the parked monitor loop on the empty→non-empty transition.
            self.shared.wake.notify_all();
        }
        drop(inner);
        CredentialLease {
            registry: Arc::downgrade(&self.shared),
            generation,
            agent: delivery.agent.clone(),
            staged_path: delivery.staged_path.clone(),
            container: container.to_string(),
        }
    }

    /// Point-in-time copy of every live lease.
    pub fn snapshot(&self) -> Vec<LeaseSnapshot> {
        let inner = self.shared.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .map(|(generation, rec)| LeaseSnapshot {
                generation: *generation,
                agent: rec.agent.clone(),
                spec_agent: rec.spec_agent,
                staged_path: rec.staged_path.clone(),
                staged_root: rec.staged_root.clone(),
                container: rec.container.clone(),
            })
            .collect()
    }

    /// Is this generation still registered? Re-checked immediately before every
    /// write; a mismatch is a SKIP (INV-7).
    pub fn is_live(&self, generation: LeaseGeneration) -> bool {
        self.shared
            .inner
            .lock()
            .unwrap()
            .entries
            .contains_key(&generation)
    }

    pub fn is_empty(&self) -> bool {
        self.shared.inner.lock().unwrap().entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.shared.inner.lock().unwrap().entries.len()
    }

    /// Clone the shared handle for the monitor's background thread (parking and
    /// shutdown only).
    pub(super) fn shared(&self) -> Arc<RegistryShared> {
        Arc::clone(&self.shared)
    }

    /// Request the monitor loop to stop (used by the monitor's `Drop`).
    pub(super) fn request_shutdown(&self) {
        self.shared.request_shutdown();
    }
}
