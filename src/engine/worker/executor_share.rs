//! The worker feature's executor share, alternating pushed and pulled jobs.
//!
//! Both classes draw on the one `daemon.features.worker.maxConcurrentJobs`
//! share of the host executor (which also enforces `daemon.maxConcurrentJobs`).
//! When both classes have waiters, grants alternate strictly; within a class
//! they are FIFO; a class with no waiters never blocks the other.
//!
//! Every waiter asks the host executor for a `Worker` permit. Whichever
//! waiter receives one routes it to the waiter whose turn it is (possibly
//! itself), so the host executor's FIFO and cancellation rules still apply.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::data::daemon_state::FeatureName;
use crate::data::worker::JobClass;
use crate::engine::daemon::executor::{Permit, SharedExecutor};

struct ShareWaiter {
    id: u64,
    tx: oneshot::Sender<WorkerPermit>,
}

#[derive(Default)]
struct ShareState {
    waiting: [VecDeque<ShareWaiter>; 2],
    running: [usize; 2],
    last: Option<JobClass>,
    next_id: u64,
}

fn slot(class: JobClass) -> usize {
    match class {
        JobClass::Push => 0,
        JobClass::Pull => 1,
    }
}

pub struct WorkerExecutorShare {
    executor: Arc<SharedExecutor>,
    state: Mutex<ShareState>,
}

/// One running job's slot. Dropping it frees the slot.
pub struct WorkerPermit {
    share: Arc<WorkerExecutorShare>,
    class: JobClass,
    /// `None` only while being unwrapped by [`WorkerPermit::into_inner_permit`].
    permit: Option<Permit>,
}

impl std::fmt::Debug for WorkerPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPermit")
            .field("class", &self.class)
            .finish()
    }
}

impl WorkerPermit {
    pub fn class(&self) -> JobClass {
        self.class
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        let mut s = self.share.lock();
        s.running[slot(self.class)] -= 1;
    }
}

/// Point-in-time counts of the share, for status views and tests
/// (`tests/worker_mixed.rs`, `tests/worker_pull_auto.rs`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerShareSnapshot {
    pub waiting_push: usize,
    pub waiting_pull: usize,
    pub running_push: usize,
    pub running_pull: usize,
}

struct WaiterGuard<'a> {
    share: &'a WorkerExecutorShare,
    class: JobClass,
    id: u64,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.share.lock().waiting[slot(self.class)].retain(|w| w.id != self.id);
    }
}

impl WorkerExecutorShare {
    pub fn new(executor: Arc<SharedExecutor>) -> Arc<Self> {
        Arc::new(Self {
            executor,
            state: Mutex::new(ShareState::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ShareState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait for a slot for a job of `class`. Cancel-safe: dropping the future
    /// gives up the place in line without leaking a slot.
    pub async fn acquire(self: &Arc<Self>, class: JobClass) -> WorkerPermit {
        let (tx, mut rx) = oneshot::channel();
        let id = {
            let mut s = self.lock();
            let id = s.next_id;
            s.next_id += 1;
            s.waiting[slot(class)].push_back(ShareWaiter { id, tx });
            id
        };
        let _guard = WaiterGuard {
            share: self,
            class,
            id,
        };
        loop {
            tokio::select! {
                biased;
                granted = &mut rx => {
                    return granted.expect("a routed permit is sent before its waiter is dropped");
                }
                permit = self.executor.acquire(FeatureName::Worker) => {
                    if let Some(mine) = self.route(permit, id) {
                        return mine;
                    }
                }
            }
        }
    }

    /// Hand `permit` to the waiter whose turn it is. Returns it wrapped when
    /// that waiter is `me`; otherwise sends it (or drops it when nobody waits).
    fn route(self: &Arc<Self>, permit: Permit, me: u64) -> Option<WorkerPermit> {
        let mut permit = Some(permit);
        loop {
            let (class, waiter) = {
                let mut s = self.lock();
                let class = match s.last {
                    Some(last) if !s.waiting[slot(last.other())].is_empty() => last.other(),
                    _ => match JobClass::ALL
                        .into_iter()
                        .find(|c| !s.waiting[slot(*c)].is_empty())
                    {
                        Some(class) => {
                            // Prefer the class that did not go last.
                            let other = class.other();
                            if s.last == Some(class) && !s.waiting[slot(other)].is_empty() {
                                other
                            } else {
                                class
                            }
                        }
                        None => return None,
                    },
                };
                let waiter = s.waiting[slot(class)].pop_front().expect("non-empty queue");
                s.running[slot(class)] += 1;
                s.last = Some(class);
                (class, waiter)
            };
            let wrapped = WorkerPermit {
                share: self.clone(),
                class,
                permit: Some(permit.take().expect("permit is routed once")),
            };
            if waiter.id == me {
                return Some(wrapped);
            }
            match waiter.tx.send(wrapped) {
                Ok(()) => return None,
                // That waiter was cancelled: release its running count, keep
                // the host permit and route it to the next waiter.
                Err(returned) => permit = Some(returned.into_inner_permit()),
            }
        }
    }

    pub fn snapshot(&self) -> WorkerShareSnapshot {
        let s = self.lock();
        WorkerShareSnapshot {
            waiting_push: s.waiting[0].len(),
            waiting_pull: s.waiting[1].len(),
            running_push: s.running[0],
            running_pull: s.running[1],
        }
    }

    /// The host executor this share draws on.
    pub fn executor(&self) -> &Arc<SharedExecutor> {
        &self.executor
    }
}

impl WorkerPermit {
    /// Release the running count but keep the host permit.
    fn into_inner_permit(mut self) -> Permit {
        self.permit.take().expect("permit present until unwrapped")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn share(cap: usize) -> Arc<WorkerExecutorShare> {
        let executor = SharedExecutor::new(cap);
        executor.set_share(FeatureName::Worker, cap);
        WorkerExecutorShare::new(executor)
    }

    /// Spawn a job that records its label when granted and holds the slot
    /// until released.
    fn job(
        share: &Arc<WorkerExecutorShare>,
        class: JobClass,
        label: &'static str,
        granted: &mpsc::UnboundedSender<&'static str>,
    ) -> oneshot::Sender<()> {
        let (release, wait) = oneshot::channel::<()>();
        let (share, granted) = (share.clone(), granted.clone());
        tokio::spawn(async move {
            let permit = share.acquire(class).await;
            granted.send(label).unwrap();
            let _ = wait.await;
            drop(permit);
        });
        release
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<&'static str>) -> &'static str {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a grant")
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn alternates_between_push_and_pull_classes_under_contention() {
        let share = share(1);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let first = job(&share, JobClass::Push, "push-0", &tx);
        assert_eq!(next(&mut rx).await, "push-0");
        let mut releases = Vec::new();
        for label in ["push-1", "push-2", "push-3"] {
            releases.push((label, job(&share, JobClass::Push, label, &tx)));
            settle().await;
        }
        for label in ["pull-1", "pull-2"] {
            releases.push((label, job(&share, JobClass::Pull, label, &tx)));
            settle().await;
        }
        assert_eq!(
            share.snapshot(),
            WorkerShareSnapshot {
                waiting_push: 3,
                waiting_pull: 2,
                running_push: 1,
                running_pull: 0
            }
        );
        first.send(()).unwrap();
        let mut order = Vec::new();
        for _ in 0..5 {
            let label = next(&mut rx).await;
            order.push(label);
            let index = releases.iter().position(|(l, _)| *l == label).unwrap();
            releases.remove(index).1.send(()).unwrap();
        }
        assert_eq!(order, ["pull-1", "push-1", "pull-2", "push-2", "push-3"]);
        settle().await;
        assert_eq!(share.snapshot(), WorkerShareSnapshot::default());
    }

    #[tokio::test(start_paused = true)]
    async fn single_class_is_never_starved() {
        let share = share(2);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let a = job(&share, JobClass::Pull, "pull-a", &tx);
        let b = job(&share, JobClass::Pull, "pull-b", &tx);
        let c = job(&share, JobClass::Pull, "pull-c", &tx);
        let mut got = vec![next(&mut rx).await, next(&mut rx).await];
        got.sort();
        assert_eq!(got, ["pull-a", "pull-b"]);
        assert_eq!(share.snapshot().running_pull, 2);
        a.send(()).unwrap();
        assert_eq!(next(&mut rx).await, "pull-c");
        drop((b, c));
    }

    #[tokio::test(start_paused = true)]
    async fn permits_release_on_drop() {
        let share = share(1);
        let permit = share.acquire(JobClass::Push).await;
        assert_eq!(permit.class(), JobClass::Push);
        assert_eq!(share.snapshot().running_push, 1);
        // A cancelled waiter leaves no trace.
        let waiting =
            tokio::time::timeout(Duration::from_millis(20), share.acquire(JobClass::Pull)).await;
        assert!(waiting.is_err());
        assert_eq!(share.snapshot().waiting_pull, 0);
        drop(permit);
        assert_eq!(share.snapshot(), WorkerShareSnapshot::default());
        let again = tokio::time::timeout(Duration::from_secs(1), share.acquire(JobClass::Pull))
            .await
            .expect("slot was freed");
        assert_eq!(
            share.executor().snapshot().per_feature[&FeatureName::Worker].running,
            1
        );
        drop(again);
        assert_eq!(
            share.executor().snapshot().per_feature[&FeatureName::Worker].running,
            0
        );
    }
}
