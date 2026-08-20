use super::*;
use crate::agent::AgentRunCancellation;
use crate::channel::{
    Channel, ChannelDelivery, ChannelReply, ChannelRun, ChannelRunContext, DeliveryDisposition,
    RunEvent,
};
use crate::config::RuntimeConfig;
use crate::daemon::execution::SchedulerAdmissionError;
use anyhow::{Result, bail};
use std::collections::{HashMap, VecDeque};
use std::future::pending;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

fn runtime(max_in_flight_runs: usize, max_concurrent_runs: usize) -> RuntimeConfig {
    RuntimeConfig {
        max_in_flight_tasks: 8,
        max_in_flight_runs,
        max_concurrent_runs,
    }
}

fn scope(agent: &str, session: &str, workspace: &str) -> super::super::ExecutionScope {
    super::super::ExecutionScope::new(
        "lark",
        session,
        SessionKey::new(agent, IsolationScope::session("lark", session)),
        PathBuf::from(workspace),
    )
}

#[tokio::test]
async fn scheduler_admits_batches_atomically_and_releases_capacity_on_completion() {
    let scheduler = super::super::ExecutionScheduler::new(&runtime(2, 2));
    let mut admitted = scheduler
        .try_enqueue_batch(vec![
            scope("codex", "chat-1", "/tmp/workspace-a"),
            scope("codex", "chat-1", "/tmp/workspace-a"),
        ])
        .unwrap();
    let (first_ticket, first_completion) = admitted.remove(0).into_parts();
    let (second_ticket, second_completion) = admitted.remove(0).into_parts();

    assert_eq!(first_ticket.ahead(), 0);
    assert_eq!(second_ticket.ahead(), 1);
    assert_eq!(
        scheduler
            .try_enqueue_batch(vec![scope("reviewer", "chat-2", "/tmp/workspace-b")])
            .err()
            .unwrap(),
        SchedulerAdmissionError::Capacity {
            current: 2,
            requested: 1,
            limit: 2,
        }
    );
    assert_eq!(
        scheduler
            .try_enqueue_batch(vec![
                scope("reviewer", "chat-2", "/tmp/workspace-b"),
                scope("reviewer", "chat-3", "/tmp/workspace-c"),
            ])
            .err()
            .unwrap(),
        SchedulerAdmissionError::Capacity {
            current: 2,
            requested: 2,
            limit: 2,
        }
    );

    drop(first_completion);
    let third = scheduler
        .try_enqueue_batch(vec![scope("codex", "chat-1", "/tmp/workspace-a")])
        .unwrap()
        .pop()
        .unwrap();
    let (third_ticket, third_completion) = third.into_parts();
    assert_eq!(third_ticket.ahead(), 2);

    drop(first_ticket);
    drop(second_ticket);
    drop(second_completion);
    drop(third_ticket);
    drop(third_completion);
}

#[tokio::test]
async fn scheduler_close_interrupts_runs_rejects_admission_and_waits_for_completion() {
    let scheduler = super::super::ExecutionScheduler::new(&runtime(4, 2));
    let mut admitted = scheduler
        .try_enqueue_batch(vec![
            scope("codex", "chat-1", "/tmp/workspace-a"),
            scope("reviewer", "chat-2", "/tmp/workspace-b"),
        ])
        .unwrap();
    let (first_ticket, first_completion) = admitted.remove(0).into_parts();
    let (second_ticket, second_completion) = admitted.remove(0).into_parts();
    let first_control = first_ticket.control();
    let second_control = second_ticket.control();

    assert_eq!(scheduler.close_and_interrupt(), 2);
    assert_eq!(
        first_control.cancelled().await,
        AgentRunCancellation::Interrupted
    );
    assert_eq!(
        second_control.cancelled().await,
        AgentRunCancellation::Interrupted
    );
    assert_eq!(
        scheduler
            .try_enqueue_batch(vec![scope("codex", "chat-3", "/tmp/workspace-c")])
            .err()
            .unwrap(),
        SchedulerAdmissionError::Closed
    );
    let barrier_key = SessionKey::new("codex", IsolationScope::session("lark", "chat-1"));
    assert!(matches!(
        scheduler.barrier(&barrier_key),
        Err(SchedulerAdmissionError::Closed)
    ));

    assert!(
        timeout(Duration::from_millis(20), scheduler.wait_until_complete())
            .await
            .is_err()
    );
    drop(first_completion);
    assert!(
        timeout(Duration::from_millis(20), scheduler.wait_until_complete())
            .await
            .is_err()
    );
    drop(second_completion);
    timeout(Duration::from_millis(20), scheduler.wait_until_complete())
        .await
        .unwrap();

    drop(first_ticket);
    drop(second_ticket);
}

#[tokio::test]
async fn scheduler_serializes_the_same_workspace_across_session_keys() {
    let scheduler = super::super::ExecutionScheduler::new(&runtime(4, 2));
    let mut admitted = scheduler
        .try_enqueue_batch(vec![
            scope("codex", "chat-1", "/tmp/shared-workspace"),
            scope("reviewer", "chat-2", "/tmp/shared-workspace"),
        ])
        .unwrap();
    let (first, _first_completion) = admitted.remove(0).into_parts();
    let (second, _second_completion) = admitted.remove(0).into_parts();

    let first_lease = first.acquire_resources().await.unwrap();
    let mut second_acquire = Box::pin(second.acquire_resources());
    assert!(
        timeout(Duration::from_millis(20), second_acquire.as_mut())
            .await
            .is_err()
    );
    drop(first_lease);
    timeout(Duration::from_millis(20), second_acquire)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn scheduler_allows_distinct_workspaces_up_to_the_global_limit() {
    let scheduler = super::super::ExecutionScheduler::new(&runtime(4, 2));
    let mut admitted = scheduler
        .try_enqueue_batch(vec![
            scope("codex", "chat-1", "/tmp/workspace-a"),
            scope("reviewer", "chat-2", "/tmp/workspace-b"),
        ])
        .unwrap();
    let (first, _first_completion) = admitted.remove(0).into_parts();
    let (second, _second_completion) = admitted.remove(0).into_parts();

    let first_lease = first.acquire_resources().await.unwrap();
    let second_lease = timeout(Duration::from_millis(20), second.acquire_resources())
        .await
        .unwrap()
        .unwrap();

    drop(first_lease);
    drop(second_lease);
}

#[tokio::test]
async fn cancelled_global_wait_releases_its_workspace_permit() {
    let scheduler = super::super::ExecutionScheduler::new(&runtime(4, 1));
    let mut admitted = scheduler
        .try_enqueue_batch(vec![
            scope("codex", "chat-1", "/tmp/workspace-a"),
            scope("reviewer", "chat-2", "/tmp/workspace-b"),
            scope("tester", "chat-3", "/tmp/workspace-b"),
        ])
        .unwrap();
    let (first, _first_completion) = admitted.remove(0).into_parts();
    let (waiting, _waiting_completion) = admitted.remove(0).into_parts();
    let (replacement, _replacement_completion) = admitted.remove(0).into_parts();

    let first_lease = first.acquire_resources().await.unwrap();
    let mut waiting_acquire = Box::pin(waiting.acquire_resources());
    assert!(
        timeout(Duration::from_millis(20), waiting_acquire.as_mut())
            .await
            .is_err()
    );
    drop(waiting_acquire);
    drop(first_lease);

    timeout(Duration::from_millis(20), replacement.acquire_resources())
        .await
        .unwrap()
        .unwrap();
}

#[derive(Default)]
struct ReliabilityRunState {
    events: Mutex<HashMap<String, Vec<RunEvent>>>,
    opened: Mutex<Vec<String>>,
    queued_count: AtomicUsize,
    terminal_count: AtomicUsize,
    queued_gate: Option<Arc<Semaphore>>,
    fail_initial_queued: bool,
    fail_started: bool,
    fail_output: bool,
}

impl ReliabilityRunState {
    fn events_for(&self, agent: &str) -> Vec<RunEvent> {
        self.events
            .lock()
            .unwrap()
            .get(agent)
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct ReliabilityRun {
    agent: String,
    state: Arc<ReliabilityRunState>,
}

impl ChannelRun for ReliabilityRun {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        self.state
            .events
            .lock()
            .unwrap()
            .entry(self.agent.clone())
            .or_default()
            .push(event.clone());

        match event {
            RunEvent::Queued { .. } => {
                let queued = self.state.queued_count.fetch_add(1, Ordering::AcqRel) + 1;
                if self.state.fail_initial_queued {
                    bail!("initial queued delivery failed");
                }
                if queued > 1
                    && let Some(gate) = &self.state.queued_gate
                {
                    gate.acquire().await.unwrap().forget();
                }
            }
            RunEvent::Started { .. } if self.state.fail_started => {
                bail!("started delivery failed");
            }
            RunEvent::Output(_) if self.state.fail_output => {
                bail!("output delivery failed");
            }
            RunEvent::Completed { .. }
            | RunEvent::Failed { .. }
            | RunEvent::Stopped
            | RunEvent::Interrupted => {
                self.state.terminal_count.fetch_add(1, Ordering::Release);
            }
            RunEvent::Started { .. } | RunEvent::Output(_) => {}
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ReliabilityChannel {
    tasks: Arc<Mutex<VecDeque<ChannelDelivery<ScopedTask>>>>,
    state: Arc<ReliabilityRunState>,
    replies: Arc<Mutex<Vec<ChannelReply>>>,
    reply_gate: Option<Arc<Semaphore>>,
}

impl Channel for ReliabilityChannel {
    type Task = ScopedTask;
    type Run = ReliabilityRun;

    fn name(&self) -> &str {
        "reliability"
    }

    fn identity(&self) -> ChannelIdentity {
        ChannelIdentity::new(self.name(), "test", self.name())
    }

    async fn recv(&mut self) -> Result<Option<ChannelDelivery<Self::Task>>> {
        if let Some(delivery) = self.tasks.lock().unwrap().pop_front() {
            Ok(Some(delivery))
        } else {
            pending().await
        }
    }

    async fn open_run(&self, _task: &Self::Task, context: ChannelRunContext) -> Result<Self::Run> {
        self.state
            .opened
            .lock()
            .unwrap()
            .push(context.agent.name.clone());
        Ok(ReliabilityRun {
            agent: context.agent.name,
            state: Arc::clone(&self.state),
        })
    }

    async fn reply(&self, _task: &Self::Task, reply: ChannelReply) -> Result<()> {
        self.replies.lock().unwrap().push(reply);
        if let Some(gate) = &self.reply_gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(())
    }
}

fn delivery(
    task_id: &str,
    session_id: &str,
) -> (
    ChannelDelivery<ScopedTask>,
    oneshot::Receiver<DeliveryDisposition>,
) {
    let (sender, receiver) = oneshot::channel();
    (
        ChannelDelivery::new(
            ScopedTask::new(task_id, session_id),
            tokio::time::Instant::now() + Duration::from_secs(3),
            move |disposition| {
                let _ = sender.send(disposition);
            },
        ),
        receiver,
    )
}

fn reliability_channel(
    delivery: ChannelDelivery<ScopedTask>,
    state: Arc<ReliabilityRunState>,
) -> ReliabilityChannel {
    ReliabilityChannel {
        tasks: Arc::new(Mutex::new(VecDeque::from([delivery]))),
        state,
        replies: Arc::new(Mutex::new(Vec::new())),
        reply_gate: None,
    }
}

fn reliability_agent(
    name: &str,
    workspace: &std::path::Path,
    path: &std::path::Path,
) -> ConfiguredAgent {
    let mut config = custom_agent(name);
    config.isolate = IsolateMode::Session;
    config.workspace = workspace.to_string_lossy().into_owned();
    config.path = path.to_string_lossy().into_owned();
    ConfiguredAgent::from_config(config).unwrap()
}

fn reliability_dispatcher(directory: &std::path::Path, runtime: &RuntimeConfig) -> AgentDispatcher {
    let store = SessionStore::open(directory.join("reliability-store.db")).unwrap();
    AgentDispatcher::from_parts(store, super::super::ExecutionScheduler::new(runtime))
}

fn run_reliability_channel(
    channel: ReliabilityChannel,
    agents: Vec<ConfiguredAgent>,
    dispatcher: AgentDispatcher,
    task_slots: super::super::TaskSlots,
) -> JoinHandle<Result<()>> {
    let commands = Arc::new(
        CommandRuntime::new(dispatcher.store.clone(), dispatcher.scheduler.clone()).unwrap(),
    );
    tokio::spawn(Daemon::run_channel(
        channel, agents, dispatcher, commands, task_slots,
    ))
}

async fn wait_for(condition: impl Fn() -> bool) {
    timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn task_capacity_sends_busy_and_accepts_only_after_the_reply_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = runtime(4, 2);
    let dispatcher = reliability_dispatcher(temp.path(), &runtime);
    let state = Arc::new(ReliabilityRunState::default());
    let (delivery, mut disposition) = delivery("busy-task", "chat-1");
    let mut channel = reliability_channel(delivery, state);
    let reply_gate = Arc::new(Semaphore::new(0));
    channel.reply_gate = Some(Arc::clone(&reply_gate));
    let replies = Arc::clone(&channel.replies);
    let task_slots = super::super::TaskSlots::new(1);
    let occupied = task_slots.try_acquire().unwrap();
    let daemon = run_reliability_channel(channel, Vec::new(), dispatcher, task_slots);

    wait_for(|| !replies.lock().unwrap().is_empty()).await;
    assert!(
        timeout(Duration::from_millis(20), &mut disposition)
            .await
            .is_err()
    );
    assert_eq!(
        replies.lock().unwrap().as_slice(),
        [ChannelReply::new(crate::i18n::NODE_BUSY)]
    );

    reply_gate.add_permits(1);
    assert_eq!(disposition.await.unwrap(), DeliveryDisposition::Accepted);
    drop(occupied);
    daemon.abort();
}

#[tokio::test]
async fn run_capacity_rejects_the_complete_fanout_and_returns_busy() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = runtime(1, 1);
    let dispatcher = reliability_dispatcher(temp.path(), &runtime);
    let state = Arc::new(ReliabilityRunState::default());
    let (delivery, disposition) = delivery("fanout", "chat-1");
    let channel = reliability_channel(delivery, Arc::clone(&state));
    let replies = Arc::clone(&channel.replies);
    let agents = vec![
        reliability_agent("codex", temp.path(), std::path::Path::new("/bin/cat")),
        reliability_agent("reviewer", temp.path(), std::path::Path::new("/bin/cat")),
    ];
    let daemon =
        run_reliability_channel(channel, agents, dispatcher, super::super::TaskSlots::new(4));

    assert_eq!(disposition.await.unwrap(), DeliveryDisposition::Accepted);
    assert_eq!(
        replies.lock().unwrap().as_slice(),
        [ChannelReply::new(crate::i18n::NODE_BUSY)]
    );
    assert!(state.opened.lock().unwrap().is_empty());
    daemon.abort();
}

#[tokio::test]
async fn receipt_waits_for_every_initial_queued_publication() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = runtime(4, 2);
    let dispatcher = reliability_dispatcher(temp.path(), &runtime);
    let queued_gate = Arc::new(Semaphore::new(0));
    let state = Arc::new(ReliabilityRunState {
        queued_gate: Some(Arc::clone(&queued_gate)),
        ..Default::default()
    });
    let (delivery, mut disposition) = delivery("visible", "chat-1");
    let channel = reliability_channel(delivery, Arc::clone(&state));
    let agents = vec![
        reliability_agent("codex", temp.path(), std::path::Path::new("/bin/cat")),
        reliability_agent("reviewer", temp.path(), std::path::Path::new("/bin/cat")),
    ];
    let daemon =
        run_reliability_channel(channel, agents, dispatcher, super::super::TaskSlots::new(4));

    wait_for(|| state.queued_count.load(Ordering::Acquire) == 2).await;
    assert!(
        timeout(Duration::from_millis(20), &mut disposition)
            .await
            .is_err()
    );
    queued_gate.add_permits(2);
    assert_eq!(disposition.await.unwrap(), DeliveryDisposition::Accepted);
    wait_for(|| state.terminal_count.load(Ordering::Acquire) == 2).await;

    for agent in ["codex", "reviewer"] {
        let events = state.events_for(agent);
        assert_eq!(events.first(), Some(&RunEvent::Queued { ahead: 0 }));
        assert!(
            events
                .iter()
                .skip(1)
                .any(|event| matches!(event, RunEvent::Started { .. }))
        );
    }
    daemon.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn failed_initial_publication_retries_without_invoking_the_backend() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let invocation = temp.path().join("invoked");
    let script = temp.path().join("agent");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf invoked > '{}'\ncat >/dev/null\n",
            invocation.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let runtime = runtime(2, 1);
    let dispatcher = reliability_dispatcher(temp.path(), &runtime);
    let state = Arc::new(ReliabilityRunState {
        fail_initial_queued: true,
        ..Default::default()
    });
    let (delivery, disposition) = delivery("failed-visible", "chat-1");
    let channel = reliability_channel(delivery, state);
    let daemon = run_reliability_channel(
        channel,
        vec![reliability_agent("codex", temp.path(), &script)],
        dispatcher,
        super::super::TaskSlots::new(2),
    );

    assert_eq!(disposition.await.unwrap(), DeliveryDisposition::Retry);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!invocation.exists());
    daemon.abort();
}

#[tokio::test]
async fn nonterminal_delivery_failures_do_not_cancel_backend_execution() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = runtime(2, 1);
    let dispatcher = reliability_dispatcher(temp.path(), &runtime);
    let state = Arc::new(ReliabilityRunState {
        fail_started: true,
        fail_output: true,
        ..Default::default()
    });
    let (delivery, disposition) = delivery("delivery-isolation", "chat-1");
    let channel = reliability_channel(delivery, Arc::clone(&state));
    let daemon = run_reliability_channel(
        channel,
        vec![reliability_agent(
            "codex",
            temp.path(),
            std::path::Path::new("/bin/cat"),
        )],
        dispatcher,
        super::super::TaskSlots::new(2),
    );

    assert_eq!(disposition.await.unwrap(), DeliveryDisposition::Accepted);
    wait_for(|| state.terminal_count.load(Ordering::Acquire) == 1).await;
    assert!(
        state
            .events_for("codex")
            .iter()
            .any(|event| matches!(event, RunEvent::Completed { exit_code: 0 }))
    );
    daemon.abort();
}
