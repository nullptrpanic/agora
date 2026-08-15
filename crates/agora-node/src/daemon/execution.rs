use crate::agent::AgentRunControl;
use crate::store::SessionKey;
use anyhow::{Result, anyhow};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExecutionScope {
    channel_name: String,
    session_id: String,
    session_key: SessionKey,
}

impl ExecutionScope {
    pub(super) fn new(
        channel_name: impl Into<String>,
        session_id: impl Into<String>,
        session_key: SessionKey,
    ) -> Self {
        Self {
            channel_name: channel_name.into(),
            session_id: session_id.into(),
            session_key,
        }
    }
}

struct ExecutionEntry {
    channel_name: String,
    session_id: String,
    control: AgentRunControl,
}

struct ScheduledEntry {
    id: u64,
    ahead: watch::Sender<usize>,
    work: ScheduledWork,
}

enum ScheduledWork {
    Execution(ExecutionEntry),
    Barrier,
}

impl ScheduledWork {
    fn execution(&self) -> Option<&ExecutionEntry> {
        match self {
            Self::Execution(execution) => Some(execution),
            Self::Barrier => None,
        }
    }
}

#[derive(Default)]
struct SchedulerState {
    next_id: u64,
    execution_count: usize,
    queues: HashMap<SessionKey, VecDeque<ScheduledEntry>>,
}

struct ExecutionSchedulerInner {
    state: Mutex<SchedulerState>,
    execution_count_tx: watch::Sender<usize>,
}

#[derive(Clone)]
pub(super) struct ExecutionScheduler {
    inner: Arc<ExecutionSchedulerInner>,
}

impl Default for ExecutionScheduler {
    fn default() -> Self {
        let (execution_count_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(ExecutionSchedulerInner {
                state: Mutex::new(SchedulerState::default()),
                execution_count_tx,
            }),
        }
    }
}

impl ExecutionScheduler {
    pub(super) fn enqueue(&self, scope: ExecutionScope) -> ExecutionTicket {
        let ExecutionScope {
            channel_name,
            session_id,
            session_key,
        } = scope;
        let control = AgentRunControl::new();
        let queue = self.insert(
            session_key,
            ScheduledWork::Execution(ExecutionEntry {
                channel_name,
                session_id,
                control: control.clone(),
            }),
        );
        ExecutionTicket { queue, control }
    }

    pub(super) fn barrier(&self, key: &SessionKey) -> ExecutionBarrier {
        ExecutionBarrier {
            queue: self.insert(key.clone(), ScheduledWork::Barrier),
        }
    }

    pub(super) fn stop(
        &self,
        channel_name: &str,
        session_id: &str,
        agent_name: Option<&str>,
    ) -> Vec<String> {
        let state = self.state();
        let mut stopped = BTreeSet::new();
        for (key, queue) in &state.queues {
            let matches_agent = agent_name
                .map(|name| name == key.agent_name())
                .unwrap_or(true);
            if !matches_agent {
                continue;
            }
            for execution in queue.iter().filter_map(|entry| entry.work.execution()) {
                if execution.channel_name == channel_name && execution.session_id == session_id {
                    execution.control.stop();
                    stopped.insert(key.agent_name().to_string());
                }
            }
        }
        stopped.into_iter().collect()
    }

    pub(super) fn stop_session_keys(&self, session_keys: &[SessionKey]) -> Vec<String> {
        let state = self.state();
        let mut stopped = BTreeSet::new();
        for key in session_keys {
            let Some(queue) = state.queues.get(key) else {
                continue;
            };
            for execution in queue.iter().filter_map(|entry| entry.work.execution()) {
                execution.control.stop();
                stopped.insert(key.agent_name().to_string());
            }
        }
        stopped.into_iter().collect()
    }

    pub(super) fn interrupt_all(&self) -> usize {
        let state = self.state();
        for execution in state
            .queues
            .values()
            .flatten()
            .filter_map(|entry| entry.work.execution())
        {
            execution.control.interrupt();
        }
        state.execution_count
    }

    pub(super) async fn wait_until_empty(&self) {
        let mut execution_count = self.inner.execution_count_tx.subscribe();
        while *execution_count.borrow() > 0 {
            if execution_count.changed().await.is_err() {
                return;
            }
        }
    }

    fn insert(&self, key: SessionKey, work: ScheduledWork) -> QueueTicket {
        let mut state = self.state();
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        let queue = state.queues.entry(key.clone()).or_default();
        let (ahead, receiver) = watch::channel(queue.len());
        let is_execution = matches!(&work, ScheduledWork::Execution(_));
        queue.push_back(ScheduledEntry { id, ahead, work });
        if is_execution {
            state.execution_count += 1;
            self.inner
                .execution_count_tx
                .send_replace(state.execution_count);
        }
        QueueTicket {
            id,
            key,
            scheduler: self.clone(),
            ahead: receiver,
        }
    }

    fn remove(&self, key: &SessionKey, id: u64) {
        let mut state = self.state();
        let (removed_execution, queue_empty) = {
            let Some(queue) = state.queues.get_mut(key) else {
                return;
            };
            let Some(index) = queue.iter().position(|entry| entry.id == id) else {
                return;
            };
            let removed_execution = queue
                .remove(index)
                .is_some_and(|entry| matches!(entry.work, ScheduledWork::Execution(_)));
            for (ahead, entry) in queue.iter().enumerate().skip(index) {
                entry.ahead.send_if_modified(|current| {
                    if *current == ahead {
                        false
                    } else {
                        *current = ahead;
                        true
                    }
                });
            }
            (removed_execution, queue.is_empty())
        };
        if queue_empty {
            state.queues.remove(key);
        }
        if removed_execution {
            state.execution_count = state.execution_count.saturating_sub(1);
            self.inner
                .execution_count_tx
                .send_replace(state.execution_count);
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SchedulerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct QueueTicket {
    id: u64,
    key: SessionKey,
    scheduler: ExecutionScheduler,
    ahead: watch::Receiver<usize>,
}

impl QueueTicket {
    fn ahead(&self) -> usize {
        *self.ahead.borrow()
    }

    async fn changed(&mut self) -> Result<usize> {
        self.ahead
            .changed()
            .await
            .map_err(|_| anyhow!("execution queue position channel closed"))?;
        Ok(*self.ahead.borrow())
    }

    async fn wait_until_front(&mut self) -> Result<()> {
        while self.ahead() > 0 {
            self.changed().await?;
        }
        Ok(())
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.scheduler.remove(&self.key, self.id);
    }
}

pub(super) struct ExecutionTicket {
    queue: QueueTicket,
    control: AgentRunControl,
}

impl ExecutionTicket {
    pub(super) fn control(&self) -> AgentRunControl {
        self.control.clone()
    }

    pub(super) fn ahead(&self) -> usize {
        self.queue.ahead()
    }

    pub(super) async fn changed(&mut self) -> Result<usize> {
        self.queue.changed().await
    }
}

pub(super) struct ExecutionBarrier {
    queue: QueueTicket,
}

impl ExecutionBarrier {
    #[cfg(test)]
    pub(super) fn ahead(&self) -> usize {
        self.queue.ahead()
    }

    pub(super) async fn wait_until_front(&mut self) -> Result<()> {
        self.queue.wait_until_front().await
    }
}
