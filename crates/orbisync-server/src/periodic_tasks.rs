//! Bounded, keyed periodic work. Missed ticks coalesce instead of accumulating.
use std::collections::{HashMap, HashSet, VecDeque};

use orbisync_domain::InstanceId;
use tokio::task::{Id, JoinError, JoinSet};

/// `None` is the generation projection inventory; instances share their key
/// across tick, checkpoint, reap, and projection retries.
pub(crate) type Key = Option<InstanceId>;

#[derive(Clone, Copy, Default)]
pub(crate) struct Work {
    pub tick: bool,
    pub checkpoint: bool,
    pub project: bool,
}

pub(crate) struct PeriodicTasks {
    limit: usize,
    pending: HashMap<Key, Work>,
    queue: VecDeque<Key>,
    active: HashSet<Key>,
    owners: HashMap<Id, Key>,
    tasks: JoinSet<Vec<InstanceId>>,
}

impl PeriodicTasks {
    pub fn new(limit: usize) -> Self {
        assert!(limit > 0);
        Self {
            limit,
            pending: HashMap::new(),
            queue: VecDeque::new(),
            active: HashSet::new(),
            owners: HashMap::new(),
            tasks: JoinSet::new(),
        }
    }

    pub fn enqueue(&mut self, key: Key, work: Work) {
        self.pending
            .entry(key)
            .and_modify(|pending| {
                pending.tick |= work.tick;
                pending.checkpoint |= work.checkpoint;
                pending.project |= work.project;
            })
            .or_insert_with(|| {
                self.queue.push_back(key);
                work
            });
    }

    pub fn start_ready<F, Fut>(&mut self, mut run: F)
    where
        F: FnMut(Key, Work) -> Fut,
        Fut: Future<Output = Vec<InstanceId>> + Send + 'static,
    {
        // Inspect at most one queue rotation: an active instance cannot block
        // unrelated work, nor monopolize newly available slots.
        for _ in 0..self.queue.len() {
            if self.tasks.len() >= self.limit {
                break;
            }
            let Some(key) = self.queue.pop_front() else {
                break;
            };
            if self.active.contains(&key) {
                self.queue.push_back(key);
                continue;
            }
            let Some(work) = self.pending.remove(&key) else {
                continue;
            };
            self.active.insert(key);
            let task = self.tasks.spawn(run(key, work));
            self.owners.insert(task.id(), key);
        }
    }

    pub fn is_running(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub async fn join_next(&mut self) -> Option<Result<Vec<InstanceId>, JoinError>> {
        let joined = self.tasks.join_next_with_id().await?;
        let id = match &joined {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        if let Some(key) = self.owners.remove(&id) {
            self.active.remove(&key);
        }
        Some(joined.map(|(_, projections)| projections))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn bounded_work_coalesces_and_preserves_instance_order() {
        let a = Some(InstanceId::generate());
        let b = Some(InstanceId::generate());
        let c = Some(InstanceId::generate());
        let blocked = Arc::new(Semaphore::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let run = |key, work: Work| {
            let blocked = blocked.clone();
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push((key, work.checkpoint));
                if key == a && !work.checkpoint {
                    blocked.acquire().await.unwrap().forget();
                }
                Vec::new()
            }
        };
        let mut tasks = PeriodicTasks::new(2);
        for key in [a, b, c] {
            tasks.enqueue(
                key,
                Work {
                    tick: true,
                    ..Work::default()
                },
            );
        }
        tasks.start_ready(&run);
        assert_eq!(tasks.tasks.len(), 2);
        for i in 0..100 {
            tasks.enqueue(
                a,
                Work {
                    tick: true,
                    checkpoint: i == 0,
                    project: false,
                },
            );
        }
        assert_eq!(tasks.pending.len(), 2); // c and one coalesced a
        tasks.join_next().await.unwrap().unwrap(); // b, while a remains blocked
        tasks.start_ready(&run);
        tasks.join_next().await.unwrap().unwrap(); // c does not wait for a
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| *key == a)
                .count(),
            1
        );
        tasks.start_ready(&run);
        assert_eq!(tasks.tasks.len(), 1); // never a second concurrent a
        blocked.add_permits(1);
        tasks.join_next().await.unwrap().unwrap();
        tasks.start_ready(&run);
        tasks.join_next().await.unwrap().unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(a, false), (b, false), (c, false), (a, true)]
        );
    }

    #[tokio::test]
    async fn drain_finishes_accepted_work_without_starting_pending_ticks() {
        let mut tasks = PeriodicTasks::new(1);
        let gate = Arc::new(Semaphore::new(0));
        tasks.enqueue(None, Work::default());
        let waiter = gate.clone();
        tasks.start_ready(move |_, _| {
            let waiter = waiter.clone();
            async move {
                waiter.acquire().await.unwrap().forget();
                Vec::new()
            }
        });
        tasks.enqueue(None, Work::default());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), tasks.join_next())
                .await
                .is_err()
        );
        gate.add_permits(1);
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        assert!(!tasks.is_running());
        assert_eq!(tasks.pending.len(), 1);
    }

    #[tokio::test]
    #[allow(clippy::panic)] // Intentional fault injection verifies task cleanup after a panic.
    async fn failed_task_releases_its_instance_key() {
        let mut tasks = PeriodicTasks::new(1);
        tasks.enqueue(None, Work::default());
        tasks.start_ready(|_, _| async { panic!("injected periodic failure") });
        assert!(tasks.join_next().await.unwrap().is_err());
        tasks.enqueue(None, Work::default());
        tasks.start_ready(|_, _| async { Vec::new() });
        tasks.join_next().await.unwrap().unwrap();
        assert!(!tasks.is_running());
    }
}
