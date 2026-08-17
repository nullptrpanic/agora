use crate::agent::AgentRunControl;
use crate::config::RuntimeConfig;
use crate::store::SessionKey;
use anyhow::{Result, anyhow};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExecutionScope {
    channel_name: String,
    session_id: String,
    session_key: SessionKey,
    workspace_key: PathBuf,
}

impl ExecutionScope {
    pub(super) fn new(
        channel_name: impl Into<String>,
        session_id: impl Into<String>,
        session_key: SessionKey,
        workspace_key: PathBuf,
    ) -> Self {
        Self {
            channel_name: channel_name.into(),
            session_id: session_id.into(),
            session_key,
            workspace_key,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SchedulerAdmissionError {
    Capacity {
        current: usize,
        requested: usize,
        limit: usize,
    },
    Closed,
}

impl fmt::Display for SchedulerAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity {
                current,
                requested,
                limit,
            } => write!(
                formatter,
                "scheduler capacity exhausted: current={current} requested={requested} limit={limit}"
            ),
            Self::Closed => formatter.write_str("scheduler admission is closed"),
        }
    }
}

impl std::error::Error for SchedulerAdmissionError {}

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

struct SchedulerState {
    accepting: bool,
    next_id: u64,
    completion_count: usize,
    queues: HashMap<SessionKey, VecDeque<ScheduledEntry>>,
}

impl Default for SchedulerState {
    fn default() -> Self {
        Self {
            accepting: true,
            next_id: 0,
            completion_count: 0,
            queues: HashMap::new(),
        }
    }
}

struct ExecutionSchedulerInner {
    state: Mutex<SchedulerState>,
    completion_count_tx: watch::Sender<usize>,
    max_in_flight_runs: usize,
    global_permit: Arc<Semaphore>,
    workspace_permits: Mutex<HashMap<PathBuf, Weak<Semaphore>>>,
}

#[derive(Clone)]
pub(super) struct ExecutionScheduler {
    inner: Arc<ExecutionSchedulerInner>,
}

impl Default for ExecutionScheduler {
    fn default() -> Self {
        Self::new(&RuntimeConfig::default())
    }
}

impl ExecutionScheduler {
    pub(super) fn new(runtime: &RuntimeConfig) -> Self {
        let (completion_count_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(ExecutionSchedulerInner {
                state: Mutex::new(SchedulerState::default()),
                completion_count_tx,
                max_in_flight_runs: runtime.max_in_flight_runs,
                global_permit: Arc::new(Semaphore::new(runtime.max_concurrent_runs)),
                workspace_permits: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(super) fn try_enqueue_batch(
        &self,
        scopes: Vec<ExecutionScope>,
    ) -> std::result::Result<Vec<AdmittedExecution>, SchedulerAdmissionError> {
        let requested = scopes.len();
        let mut state = self.state();
        if !state.accepting {
            return Err(SchedulerAdmissionError::Closed);
        }
        if requested
            > self
                .inner
                .max_in_flight_runs
                .saturating_sub(state.completion_count)
        {
            return Err(SchedulerAdmissionError::Capacity {
                current: state.completion_count,
                requested,
                limit: self.inner.max_in_flight_runs,
            });
        }

        let mut admitted = Vec::with_capacity(requested);
        for scope in scopes {
            let ExecutionScope {
                channel_name,
                session_id,
                session_key,
                workspace_key,
            } = scope;
            let control = AgentRunControl::new();
            let queue = self.insert_locked(
                &mut state,
                session_key,
                ScheduledWork::Execution(ExecutionEntry {
                    channel_name,
                    session_id,
                    control: control.clone(),
                }),
            );
            admitted.push(AdmittedExecution {
                ticket: ExecutionTicket {
                    queue,
                    control,
                    workspace_permit: self.workspace_permit(&workspace_key),
                    global_permit: Arc::clone(&self.inner.global_permit),
                },
                completion: RunCompletionGuard {
                    scheduler: self.clone(),
                },
            });
        }
        state.completion_count += requested;
        self.inner
            .completion_count_tx
            .send_replace(state.completion_count);
        Ok(admitted)
    }

    #[cfg(test)]
    pub(super) fn enqueue(&self, scope: ExecutionScope) -> AdmittedExecution {
        self.try_enqueue_batch(vec![scope])
            .expect("test execution admission should fit")
            .pop()
            .expect("one execution should be admitted")
    }

    pub(super) fn barrier(
        &self,
        key: &SessionKey,
    ) -> std::result::Result<ExecutionBarrier, SchedulerAdmissionError> {
        let mut state = self.state();
        if !state.accepting {
            return Err(SchedulerAdmissionError::Closed);
        }
        Ok(ExecutionBarrier {
            queue: self.insert_locked(&mut state, key.clone(), ScheduledWork::Barrier),
        })
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

    pub(super) fn close_and_interrupt(&self) -> usize {
        let mut state = self.state();
        state.accepting = false;
        for execution in state
            .queues
            .values()
            .flatten()
            .filter_map(|entry| entry.work.execution())
        {
            execution.control.interrupt();
        }
        state.completion_count
    }

    #[cfg(test)]
    pub(super) fn interrupt_all(&self) -> usize {
        let state = self.state();
        let mut interrupted = 0;
        for execution in state
            .queues
            .values()
            .flatten()
            .filter_map(|entry| entry.work.execution())
        {
            execution.control.interrupt();
            interrupted += 1;
        }
        interrupted
    }

    pub(super) async fn wait_until_complete(&self) {
        let mut completion_count = self.inner.completion_count_tx.subscribe();
        while *completion_count.borrow() > 0 {
            if completion_count.changed().await.is_err() {
                return;
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn wait_until_empty(&self) {
        self.wait_until_complete().await;
    }

    fn insert_locked(
        &self,
        state: &mut SchedulerState,
        key: SessionKey,
        work: ScheduledWork,
    ) -> QueueTicket {
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        let queue = state.queues.entry(key.clone()).or_default();
        let (ahead, receiver) = watch::channel(queue.len());
        queue.push_back(ScheduledEntry { id, ahead, work });
        QueueTicket {
            id,
            key,
            scheduler: self.clone(),
            ahead: receiver,
        }
    }

    fn workspace_permit(&self, key: &Path) -> Arc<Semaphore> {
        let mut permits = self
            .inner
            .workspace_permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(permit) = permits.get(key).and_then(Weak::upgrade) {
            return permit;
        }
        let permit = Arc::new(Semaphore::new(1));
        permits.insert(key.to_path_buf(), Arc::downgrade(&permit));
        permit
    }

    fn remove(&self, key: &SessionKey, id: u64) {
        let mut state = self.state();
        let queue_empty = {
            let Some(queue) = state.queues.get_mut(key) else {
                return;
            };
            let Some(index) = queue.iter().position(|entry| entry.id == id) else {
                return;
            };
            queue.remove(index);
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
            queue.is_empty()
        };
        if queue_empty {
            state.queues.remove(key);
        }
    }

    fn complete(&self) {
        let mut state = self.state();
        state.completion_count = state.completion_count.saturating_sub(1);
        self.inner
            .completion_count_tx
            .send_replace(state.completion_count);
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

pub(super) struct AdmittedExecution {
    ticket: ExecutionTicket,
    completion: RunCompletionGuard,
}

impl AdmittedExecution {
    pub(super) fn into_parts(self) -> (ExecutionTicket, RunCompletionGuard) {
        (self.ticket, self.completion)
    }
}

impl std::ops::Deref for AdmittedExecution {
    type Target = ExecutionTicket;

    fn deref(&self) -> &Self::Target {
        &self.ticket
    }
}

impl std::ops::DerefMut for AdmittedExecution {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ticket
    }
}

pub(super) struct RunCompletionGuard {
    scheduler: ExecutionScheduler,
}

impl Drop for RunCompletionGuard {
    fn drop(&mut self) {
        self.scheduler.complete();
    }
}

pub(super) struct ExecutionTicket {
    queue: QueueTicket,
    control: AgentRunControl,
    workspace_permit: Arc<Semaphore>,
    global_permit: Arc<Semaphore>,
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

    pub(super) async fn acquire_resources(&self) -> Result<ExecutionLease> {
        let workspace = Arc::clone(&self.workspace_permit)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("workspace execution permit closed"))?;
        let global = Arc::clone(&self.global_permit)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("global execution permit closed"))?;
        Ok(ExecutionLease {
            _workspace: workspace,
            _global: global,
        })
    }
}

pub(super) struct ExecutionLease {
    _workspace: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
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
