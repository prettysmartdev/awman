//! An in-process squad daemon task that owns the environment it was started
//! under.
//!
//! The daemon reads `AWMAN_HOME` (and friends) lazily while serving,
//! so a test must not restore the previous values until the daemon is gone.
//! [`InProcessDaemon`] aborts the task first and restores the variables
//! second, on an explicit [`InProcessDaemon::abort`] and on a panic alike.
//! Callers still serialise on their own `ENV_LOCK` for the whole test.

#![allow(dead_code)]

use std::ffi::OsString;

/// Process environment variables set for a test, restored on drop.
pub struct EnvOverride {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl EnvOverride {
    /// Set every `(name, value)`, remembering what each held before.
    pub fn set(vars: &[(&'static str, &std::ffi::OsStr)]) -> Self {
        let previous = vars
            .iter()
            .map(|(name, value)| {
                let old = std::env::var_os(name);
                std::env::set_var(name, value);
                (*name, old)
            })
            .collect();
        Self { previous }
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A spawned daemon task plus the environment it runs under. Dropping it (or
/// calling [`Self::abort`]) aborts the task, then restores the environment.
pub struct InProcessDaemon {
    task: tokio::task::JoinHandle<()>,
    _env: EnvOverride,
}

impl InProcessDaemon {
    pub fn new(task: tokio::task::JoinHandle<()>, env: EnvOverride) -> Self {
        Self { task, _env: env }
    }

    /// Stop the daemon and restore the environment it was started under.
    pub fn abort(self) {
        drop(self);
    }
}

impl Drop for InProcessDaemon {
    fn drop(&mut self) {
        // Runs before the fields drop, so the task is aborted while the
        // environment it reads is still in place.
        self.task.abort();
    }
}
