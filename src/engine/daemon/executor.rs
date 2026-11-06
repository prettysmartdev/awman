//! Shared host capacity with round-robin features and FIFO waiters.
use crate::data::daemon_state::FeatureName;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

struct Waiter {
    id: u64,
    since: Instant,
    tx: oneshot::Sender<Permit>,
}
struct State {
    cap: usize,
    shares: BTreeMap<FeatureName, usize>,
    running: BTreeMap<FeatureName, usize>,
    waiting: BTreeMap<FeatureName, VecDeque<Waiter>>,
    cursor: usize,
    next_id: u64,
}
pub struct SharedExecutor {
    inner: Mutex<State>,
}
pub struct Permit {
    executor: Arc<SharedExecutor>,
    feature: FeatureName,
    active: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureSlots {
    pub share: usize,
    pub running: usize,
    pub waiting: usize,
    pub oldest_wait: Option<Duration>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorSnapshot {
    pub cap: usize,
    pub per_feature: BTreeMap<FeatureName, FeatureSlots>,
}

impl SharedExecutor {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(State {
                cap,
                shares: BTreeMap::new(),
                running: BTreeMap::new(),
                waiting: BTreeMap::new(),
                cursor: FeatureName::ALL.len() - 1,
                next_id: 0,
            }),
        })
    }
    pub fn set_cap(self: &Arc<Self>, cap: usize) {
        let mut s = self.inner.lock().unwrap();
        s.cap = cap;
        self.schedule(&mut s);
    }
    pub fn set_share(self: &Arc<Self>, f: FeatureName, share: usize) {
        let mut s = self.inner.lock().unwrap();
        s.shares.insert(f, share);
        self.schedule(&mut s);
    }
    fn schedule(self: &Arc<Self>, s: &mut State) {
        while s.running.values().sum::<usize>() < s.cap {
            let next = (1..=FeatureName::ALL.len())
                .map(|offset| (s.cursor + offset) % FeatureName::ALL.len())
                .find(|&i| {
                    let f = FeatureName::ALL[i];
                    s.waiting.get(&f).is_some_and(|q| !q.is_empty())
                        && s.running.get(&f).copied().unwrap_or(0)
                            < s.shares.get(&f).copied().unwrap_or(0)
                });
            let Some(i) = next else { break };
            let f = FeatureName::ALL[i];
            let waiter = s.waiting.get_mut(&f).unwrap().pop_front().unwrap();
            if waiter.tx.is_closed() {
                continue;
            }
            *s.running.entry(f).or_default() += 1;
            s.cursor = i;
            let permit = Permit {
                executor: self.clone(),
                feature: f,
                active: true,
            };
            if let Err(mut permit) = waiter.tx.send(permit) {
                *s.running.get_mut(&f).unwrap() -= 1;
                permit.active = false;
            }
        }
    }
    pub async fn acquire(self: &Arc<Self>, feature: FeatureName) -> Permit {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut s = self.inner.lock().unwrap();
            let id = s.next_id;
            s.next_id += 1;
            s.waiting.entry(feature).or_default().push_back(Waiter {
                id,
                since: Instant::now(),
                tx,
            });
            self.schedule(&mut s);
            id
        };
        let _waiting = Waiting {
            executor: self.clone(),
            feature,
            id,
        };
        rx.await
            .expect("executor retains waiter until grant or cancellation")
    }
    pub fn try_acquire(self: &Arc<Self>, f: FeatureName) -> Option<Permit> {
        let mut s = self.inner.lock().unwrap();
        self.schedule(&mut s);
        if s.waiting.get(&f).is_some_and(|q| !q.is_empty())
            || s.running.values().sum::<usize>() >= s.cap
            || s.running.get(&f).copied().unwrap_or(0) >= s.shares.get(&f).copied().unwrap_or(0)
        {
            return None;
        }
        *s.running.entry(f).or_default() += 1;
        s.cursor = FeatureName::ALL.iter().position(|&x| x == f).unwrap();
        Some(Permit {
            executor: self.clone(),
            feature: f,
            active: true,
        })
    }
    pub fn snapshot(&self) -> ExecutorSnapshot {
        let s = self.inner.lock().unwrap();
        ExecutorSnapshot {
            cap: s.cap,
            per_feature: FeatureName::ALL
                .into_iter()
                .map(|f| {
                    (
                        f,
                        FeatureSlots {
                            share: s.shares.get(&f).copied().unwrap_or(0),
                            running: s.running.get(&f).copied().unwrap_or(0),
                            waiting: s.waiting.get(&f).map_or(0, VecDeque::len),
                            oldest_wait: s
                                .waiting
                                .get(&f)
                                .and_then(|q| q.front())
                                .map(|w| w.since.elapsed()),
                        },
                    )
                })
                .collect(),
        }
    }
}
struct Waiting {
    executor: Arc<SharedExecutor>,
    feature: FeatureName,
    id: u64,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        let mut s = self.executor.inner.lock().unwrap();
        if let Some(q) = s.waiting.get_mut(&self.feature) {
            q.retain(|w| w.id != self.id);
        }
        self.executor.schedule(&mut s);
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut s = self.executor.inner.lock().unwrap();
        *s.running.get_mut(&self.feature).unwrap() -= 1;
        self.executor.schedule(&mut s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{future::poll_fn, FutureExt};
    use std::future::Future;
    use std::task::Poll;
    use FeatureName::{Engine, Squad, Worker};
    async fn queued(
        executor: &Arc<SharedExecutor>,
        f: FeatureName,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Permit> + use<>>> {
        let executor = executor.clone();
        let mut future = Box::pin(async move { executor.acquire(f).await });
        poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        future
    }
    #[tokio::test]
    async fn flooded_feature_yields_round_robin_and_preserves_fifo() {
        let e = SharedExecutor::new(2);
        for f in [Worker, Squad] {
            e.set_share(f, 2);
        }
        let first = e.acquire(Worker).await;
        let second = e.acquire(Worker).await;
        let mut flood = Vec::new();
        for _ in 0..64 {
            flood.push(queued(&e, Worker).await);
        }
        let mut other = queued(&e, Squad).await;
        drop(first);
        // Cursor was worker, so squad gets the next slot despite the flood.
        let squad = other
            .as_mut()
            .now_or_never()
            .expect("squad was granted first");
        assert_eq!(e.snapshot().per_feature[&Worker].running, 1);
        drop(second);
        let worker = flood[0].as_mut().now_or_never().expect("FIFO head granted");
        assert!(flood[1].as_mut().now_or_never().is_none());
        assert_eq!(
            e.snapshot()
                .per_feature
                .values()
                .map(|s| s.running)
                .sum::<usize>(),
            2
        );
        let mut other = queued(&e, Squad).await;
        drop(worker);
        let squad2 = other
            .as_mut()
            .now_or_never()
            .expect("squad keeps progressing under load");
        drop(flood);
        drop(squad);
        drop(squad2);
        assert_eq!(
            e.snapshot()
                .per_feature
                .values()
                .map(|s| s.running + s.waiting)
                .sum::<usize>(),
            0
        );
    }
    #[tokio::test]
    async fn cancellation_and_live_shrink_do_not_leak_capacity() {
        let e = SharedExecutor::new(2);
        e.set_share(Worker, 2);
        e.set_share(Engine, 1);
        let first = e.acquire(Worker).await;
        let second = e.acquire(Worker).await;
        let waiting = queued(&e, Engine).await;
        drop(waiting);
        assert_eq!(e.snapshot().per_feature[&Engine].waiting, 0);
        e.set_cap(1);
        drop(first);
        assert!(e.try_acquire(Engine).is_none());
        drop(second);
        let engine = e.acquire(Engine).await;
        assert!(e.try_acquire(Engine).is_none());
        drop(engine);
        e.set_share(Engine, 0);
        let waiting = queued(&e, Engine).await;
        drop(waiting);
        assert_eq!(e.snapshot().per_feature[&Engine].running, 0);
    }
}
