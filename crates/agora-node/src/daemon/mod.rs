use crate::agent::{
    AgentOutput, AgentRegistry, AgentRunCancellation, AgentRunControl, AgentRunOutcome,
    AgentSessionUpdate, AgentTask, ConfiguredAgent,
};
use crate::channel::{
    Channel, ChannelAgent, ChannelReply, ChannelRun, ChannelRunContext, ChannelTask,
    ConfiguredChannel, DeliveryReceipt, InterruptCallback, RunEvent,
};
use crate::config::NodeConfig;
use crate::i18n;
use crate::instance::{NodeInstanceGuard, StatePaths};
use crate::store::{
    ChannelIdentity, SessionKey, SessionStore, StoreChannelSessionKey, StoreSessionKey,
};
use crate::task::{OutputEvent, TaskContent};
use agora_core::logger;
use anyhow::{Result, bail};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, oneshot};
use tokio::task::{JoinError, JoinSet};
use uuid::Uuid;

mod command;
mod execution;

use command::{CommandOutcome, CommandRuntime};
use execution::{ExecutionScheduler, ExecutionScope, SchedulerAdmissionError};

#[cfg(test)]
mod tests;

const CHANNEL_RETRY_DELAY: Duration = Duration::from_secs(1);
const SHUTDOWN_RUN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct TaskSlots {
    semaphore: Arc<Semaphore>,
    limit: usize,
}

impl TaskSlots {
    fn new(limit: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(limit)),
            limit,
        }
    }

    fn try_acquire(&self) -> std::result::Result<OwnedSemaphorePermit, TryAcquireError> {
        Arc::clone(&self.semaphore).try_acquire_owned()
    }

    fn current(&self) -> usize {
        self.limit
            .saturating_sub(self.semaphore.available_permits())
    }

    fn close(&self) {
        self.semaphore.close();
    }
}

#[derive(Clone)]
struct AgentDispatcher {
    store: SessionStore,
    scheduler: ExecutionScheduler,
}

impl AgentDispatcher {
    fn from_parts(store: SessionStore, scheduler: ExecutionScheduler) -> Self {
        Self { store, scheduler }
    }

    async fn start_channel_task<C>(
        &self,
        channel: &C,
        agents: Vec<ConfiguredAgent>,
        task: C::Task,
        runs: &mut JoinSet<Result<()>>,
    ) -> Result<()>
    where
        C: Channel + Sync,
        C::Task: Send + Sync + 'static,
        C::Run: Send + Sync + 'static,
    {
        let channel_identity = channel.identity();
        let agents = self.enabled_agents(&channel_identity, task.session_id(), &agents)?;
        if agents.is_empty() {
            return channel
                .reply(&task, ChannelReply::new(i18n::NO_ENABLED_AGENTS))
                .await;
        }
        let content = task
            .input()
            .message()
            .ok_or_else(|| anyhow::anyhow!("command input cannot start an agent run"))?
            .clone();
        self.start_agent_runs(channel, agents, task, content, runs)
            .await
    }

    async fn start_agent_runs<C>(
        &self,
        channel: &C,
        agents: Vec<ConfiguredAgent>,
        task: C::Task,
        content: TaskContent,
        runs: &mut JoinSet<Result<()>>,
    ) -> Result<()>
    where
        C: Channel + Sync,
        C::Task: Send + Sync + 'static,
        C::Run: Send + Sync + 'static,
    {
        let channel_identity = channel.identity();
        let planned = agents
            .into_iter()
            .map(|agent| {
                let isolation_scope = agent.isolation_scope(channel.name(), task.session_id());
                let key = SessionKey::new(agent.name(), isolation_scope);
                let store_key = agent.store_session_key(&channel_identity, task.session_id());
                (agent, key, store_key, AgentTask::new(content.clone()))
            })
            .collect::<Vec<_>>();
        let scopes = planned
            .iter()
            .map(|(agent, key, _, _)| {
                ExecutionScope::new(
                    channel.name(),
                    task.session_id(),
                    key.clone(),
                    agent.workspace_key().to_path_buf(),
                )
            })
            .collect();
        let admitted = match self.scheduler.try_enqueue_batch(scopes) {
            Ok(admitted) => admitted,
            Err(SchedulerAdmissionError::Capacity {
                current,
                requested,
                limit,
            }) => {
                logger::error!(
                    "node run capacity exhausted channel={} task={} current={} requested={} limit={}",
                    channel.name(),
                    task.task_id(),
                    current,
                    requested,
                    limit
                );
                return channel
                    .reply(&task, ChannelReply::new(i18n::NODE_BUSY))
                    .await;
            }
            Err(err @ SchedulerAdmissionError::Closed) => return Err(err.into()),
        };
        let mut prepared = Vec::with_capacity(admitted.len());

        for ((agent, _key, store_key, agent_task), admitted) in planned.into_iter().zip(admitted) {
            let (execution, completion) = admitted.into_parts();
            let control = execution.control();
            let interrupt_control = control.clone();
            let run = channel
                .open_run(
                    &task,
                    ChannelRunContext {
                        agent: ChannelAgent {
                            name: agent.name().to_string(),
                        },
                        interrupt: Some(InterruptCallback::new(move || interrupt_control.stop())),
                    },
                )
                .await?;
            let output =
                AgentRunOutput::for_task(run, channel.name(), task.task_id(), agent.name());
            prepared.push((
                agent, store_key, agent_task, execution, completion, control, output,
            ));
        }

        for (_, _, _, execution, _, _, output) in &prepared {
            output.initial_queued(execution.ahead()).await?;
        }

        for (agent, store_key, agent_task, mut execution, completion, control, mut output) in
            prepared
        {
            let dispatcher = self.clone();
            runs.spawn(async move {
                while execution.ahead() > 0 {
                    let ahead = tokio::select! {
                        ahead = execution.changed() => ahead?,
                        cancellation = control.cancelled() => {
                            drop(execution);
                            let result = output.cancelled(cancellation).await;
                            drop(completion);
                            return result;
                        }
                    };
                    if ahead > 0 {
                        tokio::select! {
                            result = output.queued(ahead) => result?,
                            cancellation = control.cancelled() => {
                                drop(execution);
                                let result = output.cancelled(cancellation).await;
                                drop(completion);
                                return result;
                            }
                        }
                    }
                }
                let lease = tokio::select! {
                    result = execution.acquire_resources() => result?,
                    cancellation = control.cancelled() => {
                        drop(execution);
                        let result = output.cancelled(cancellation).await;
                        drop(completion);
                        return result;
                    }
                };
                tokio::select! {
                    result = output.started() => result?,
                    cancellation = control.cancelled() => {
                        drop(lease);
                        drop(execution);
                        let result = output.cancelled(cancellation).await;
                        drop(completion);
                        return result;
                    }
                }
                let result = dispatcher
                    .execute_agent(&store_key, &agent, agent_task, control.clone(), &mut output)
                    .await;
                drop(lease);
                drop(execution);
                let result = match result {
                    Ok(AgentRunOutcome::Completed(outcome)) if outcome.exit_code() == 0 => {
                        output.completed(outcome.exit_code()).await
                    }
                    Ok(AgentRunOutcome::Completed(outcome)) => {
                        let err = anyhow::anyhow!(
                            "agent process exited with status {}",
                            outcome.exit_code()
                        );
                        output.failed(err.to_string()).await?;
                        Err(err)
                    }
                    Ok(AgentRunOutcome::Cancelled(cancellation)) => {
                        output.cancelled(cancellation).await
                    }
                    Err(err) => {
                        output.failed(err.to_string()).await?;
                        Err(err)
                    }
                };
                drop(completion);
                result
            });
        }
        Ok(())
    }

    fn enabled_agents(
        &self,
        channel: &ChannelIdentity,
        session_id: &str,
        agents: &[ConfiguredAgent],
    ) -> Result<Vec<ConfiguredAgent>> {
        let key = StoreChannelSessionKey::new(channel.clone(), session_id);
        let disabled = self
            .store
            .disabled_agents(&key)?
            .into_iter()
            .collect::<HashSet<_>>();
        Ok(agents
            .iter()
            .filter(|agent| !disabled.contains(agent.identity()))
            .cloned()
            .collect())
    }

    fn log_run_result(result: std::result::Result<Result<()>, JoinError>) {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => logger::error!("agent run failed: {}", err),
            Err(err) => logger::error!("agent task join failed: {}", err),
        }
    }

    async fn execute_agent<O>(
        &self,
        key: &StoreSessionKey,
        agent: &ConfiguredAgent,
        task: AgentTask,
        control: AgentRunControl,
        output: &mut O,
    ) -> Result<AgentRunOutcome>
    where
        O: AgentOutput + Send,
    {
        let mut session_id = self.store.get(key)?;
        let mut outcome = match agent
            .run(task.clone(), session_id.clone(), control.clone(), output)
            .await?
        {
            AgentRunOutcome::Completed(outcome) => outcome,
            cancelled @ AgentRunOutcome::Cancelled(_) => return Ok(cancelled),
        };

        if outcome.session_update() == &AgentSessionUpdate::NotFound {
            let Some(stale_session_id) = session_id else {
                bail!("agent reported a missing session without a resume session");
            };
            self.store.remove_if_matches(key, &stale_session_id)?;
            session_id = None;
            logger::info!(
                "agent session missing; starting a new session agent={}",
                agent.name()
            );
            outcome = match agent.run(task, None, control, output).await? {
                AgentRunOutcome::Completed(outcome) => outcome,
                cancelled @ AgentRunOutcome::Cancelled(_) => return Ok(cancelled),
            };
            if outcome.session_update() == &AgentSessionUpdate::NotFound {
                bail!("agent reported a missing session after starting without a session");
            }
        }

        match outcome.session_update() {
            AgentSessionUpdate::Set(observed_session_id) => {
                self.store
                    .observe(key, session_id.as_deref(), observed_session_id)?;
            }
            AgentSessionUpdate::Unchanged
                if session_id.is_none()
                    && outcome.exit_code() == 0
                    && agent.requires_session_id() =>
            {
                bail!("agent completed a fresh run without a session id")
            }
            AgentSessionUpdate::Unchanged | AgentSessionUpdate::NotFound => {}
        }
        Ok(AgentRunOutcome::Completed(outcome))
    }
}

pub struct Daemon {
    instance_guard: NodeInstanceGuard,
    config: NodeConfig,
    dispatcher: AgentDispatcher,
    commands: Arc<CommandRuntime>,
    task_slots: TaskSlots,
}

#[derive(Clone)]
pub struct DaemonShutdown {
    scheduler: ExecutionScheduler,
    task_slots: TaskSlots,
}

impl DaemonShutdown {
    pub async fn interrupt(&self) {
        self.task_slots.close();
        let interrupted = self.scheduler.close_and_interrupt();
        if interrupted == 0 {
            return;
        }
        logger::info!("interrupting {} agent runs before shutdown", interrupted);
        if tokio::time::timeout(SHUTDOWN_RUN_TIMEOUT, self.scheduler.wait_until_complete())
            .await
            .is_err()
        {
            logger::error!(
                "timed out waiting for agent interruption notifications after {} seconds",
                SHUTDOWN_RUN_TIMEOUT.as_secs()
            );
        }
    }
}

impl Daemon {
    pub fn new(config: NodeConfig) -> Result<Self> {
        let paths = StatePaths::from_environment()?;
        Self::new_with_paths(config, paths)
    }

    pub fn new_with_paths(mut config: NodeConfig, paths: StatePaths) -> Result<Self> {
        config.validate()?;
        config.apply_proxy_defaults();
        let instance_guard = NodeInstanceGuard::acquire(paths.clone())?;
        let store = SessionStore::open(paths.store_path())?;
        config.validate_filesystem()?;
        let scheduler = ExecutionScheduler::new(&config.runtime);
        let task_slots = TaskSlots::new(config.runtime.max_in_flight_tasks);
        Ok(Self {
            instance_guard,
            config,
            dispatcher: AgentDispatcher::from_parts(store.clone(), scheduler.clone()),
            commands: Arc::new(CommandRuntime::new(store, scheduler)?),
            task_slots,
        })
    }

    pub fn shutdown_handle(&self) -> DaemonShutdown {
        DaemonShutdown {
            scheduler: self.dispatcher.scheduler.clone(),
            task_slots: self.task_slots.clone(),
        }
    }

    pub async fn run(self) -> Result<()> {
        let Self {
            instance_guard,
            config,
            dispatcher,
            commands,
            task_slots,
        } = self;
        let _instance_guard = instance_guard;
        let NodeConfig {
            proxy: _,
            runtime: _,
            channels,
            agents,
        } = config;
        let agents = AgentRegistry::from_configs(agents)?;
        let shutdown = DaemonShutdown {
            scheduler: dispatcher.scheduler.clone(),
            task_slots: task_slots.clone(),
        };
        let mut configured_channels = Vec::new();

        for channel_config in channels {
            let Some(channel) = ConfiguredChannel::from_config(channel_config)? else {
                continue;
            };
            let subscribed_agents = agents.subscribed_to(channel.name());
            if subscribed_agents.is_empty() {
                continue;
            }
            configured_channels.push((channel, subscribed_agents));
        }

        let mut tasks = JoinSet::new();
        for (channel, subscribed_agents) in configured_channels {
            let dispatcher = dispatcher.clone();
            let commands = Arc::clone(&commands);
            let task_slots = task_slots.clone();
            tasks.spawn(async move {
                Self::run_channel(channel, subscribed_agents, dispatcher, commands, task_slots)
                    .await
            });
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    shutdown.interrupt().await;
                    return Err(err);
                }
                Err(err) => {
                    shutdown.interrupt().await;
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }

    async fn run_channel<C>(
        mut channel: C,
        agents: Vec<ConfiguredAgent>,
        dispatcher: AgentDispatcher,
        commands: Arc<CommandRuntime>,
        task_slots: TaskSlots,
    ) -> Result<()>
    where
        C: Channel + Clone + Send + Sync + 'static,
        C::Task: Send + Sync + 'static,
        C::Run: Send + Sync + 'static,
    {
        let mut routes = JoinSet::new();
        let mut route_tail = None;
        loop {
            tokio::select! {
                received = channel.recv() => match received {
                    Ok(Some(delivery)) => {
                        let (task, receipt) = delivery.into_parts();
                        let task_slot = match task_slots.try_acquire() {
                            Ok(task_slot) => task_slot,
                            Err(TryAcquireError::NoPermits) => {
                                logger::error!(
                                    "node task capacity exhausted channel={} task={} current={} limit={}",
                                    channel.name(),
                                    task.task_id(),
                                    task_slots.current(),
                                    task_slots.limit
                                );
                                Self::reply_busy(&channel, &task, receipt).await;
                                continue;
                            }
                            Err(TryAcquireError::Closed) => {
                                logger::info!(
                                    "rejecting channel task during shutdown channel={} task={}",
                                    channel.name(),
                                    task.task_id()
                                );
                                drop(receipt);
                                continue;
                            }
                        };
                        let task_channel = channel.clone();
                        let task_agents = agents.clone();
                        let task_dispatcher = dispatcher.clone();
                        let task_commands = Arc::clone(&commands);
                        let predecessor = route_tail.take();
                        let (admitted, successor) = oneshot::channel();
                        route_tail = Some(successor);
                        routes.spawn(async move {
                            let mut agent_runs = JoinSet::new();
                            let deadline = receipt.deadline();
                            let channel_name = task_channel.name().to_string();
                            let task_id = task.task_id().to_string();
                            let route_result = match tokio::time::timeout_at(deadline, async {
                                if let Some(predecessor) = predecessor {
                                    let _ = predecessor.await;
                                }
                                Self::route_channel_task(
                                    &task_channel,
                                    &task_agents,
                                    &task_dispatcher,
                                    &task_commands,
                                    task,
                                    &mut agent_runs,
                                )
                                .await
                            })
                            .await
                            {
                                Ok(result) => result,
                                Err(_) => Err(anyhow::anyhow!(
                                    "channel task admission timed out channel={channel_name} task={task_id}"
                                )),
                            };
                            if route_result.is_ok() {
                                receipt.accept();
                            } else {
                                drop(receipt);
                            }
                            let _ = admitted.send(());
                            while let Some(result) = agent_runs.join_next().await {
                                AgentDispatcher::log_run_result(result);
                            }
                            drop(task_slot);
                            route_result
                        });
                    }
                    Ok(None) => {
                        logger::error!("channel ended channel={}", channel.name());
                        tokio::time::sleep(CHANNEL_RETRY_DELAY).await;
                    }
                    Err(err) => {
                        logger::error!("channel receive failed channel={}: {}", channel.name(), err);
                        tokio::time::sleep(CHANNEL_RETRY_DELAY).await;
                    }
                },
                result = routes.join_next(), if !routes.is_empty() => {
                    if let Some(result) = result {
                        AgentDispatcher::log_run_result(result);
                    }
                },
            }
        }
    }

    async fn reply_busy<C>(channel: &C, task: &C::Task, receipt: DeliveryReceipt)
    where
        C: Channel + Sync,
    {
        let deadline = receipt.deadline();
        match tokio::time::timeout_at(
            deadline,
            channel.reply(task, ChannelReply::new(i18n::NODE_BUSY)),
        )
        .await
        {
            Ok(Ok(())) => receipt.accept(),
            Ok(Err(err)) => logger::error!(
                "failed to deliver node busy reply channel={} task={}: {}",
                channel.name(),
                task.task_id(),
                err
            ),
            Err(_) => logger::error!(
                "node busy reply timed out channel={} task={}",
                channel.name(),
                task.task_id()
            ),
        }
    }

    async fn route_channel_task<C>(
        channel: &C,
        agents: &[ConfiguredAgent],
        dispatcher: &AgentDispatcher,
        commands: &CommandRuntime,
        task: C::Task,
        runs: &mut JoinSet<Result<()>>,
    ) -> Result<()>
    where
        C: Channel + Sync,
        C::Task: Send + Sync + 'static,
        C::Run: Send + Sync + 'static,
    {
        match commands
            .handle(&channel.identity(), task.session_id(), agents, task.input())
            .await?
        {
            CommandOutcome::PassThrough => {
                dispatcher
                    .start_channel_task(channel, agents.to_vec(), task, runs)
                    .await
            }
            CommandOutcome::Reply(Some(reply)) => channel.reply(&task, reply).await,
            CommandOutcome::Reply(None) => Ok(()),
            CommandOutcome::Dispatch(dispatch) => {
                let (agents, content) = dispatch.into_parts();
                dispatcher
                    .start_agent_runs(channel, agents, task, content, runs)
                    .await
            }
        }
    }
}

struct AgentRunOutput<R> {
    run: R,
    run_id: String,
    channel_name: String,
    task_id: String,
    agent_name: String,
}

impl<R> AgentRunOutput<R>
where
    R: ChannelRun + Send + Sync,
{
    #[cfg(test)]
    fn new(run: R) -> Self {
        Self::for_task(run, "test", "test", "test")
    }

    fn for_task(run: R, channel_name: &str, task_id: &str, agent_name: &str) -> Self {
        Self {
            run,
            run_id: Uuid::new_v4().to_string(),
            channel_name: channel_name.to_string(),
            task_id: task_id.to_string(),
            agent_name: agent_name.to_string(),
        }
    }

    async fn started(&self) -> Result<()> {
        self.publish_nonterminal(
            RunEvent::Started {
                run_id: self.run_id.clone(),
            },
            "started",
        )
        .await
    }

    async fn queued(&self, ahead: usize) -> Result<()> {
        self.publish_nonterminal(RunEvent::Queued { ahead }, "queued")
            .await
    }

    async fn initial_queued(&self, ahead: usize) -> Result<()> {
        self.run.publish(RunEvent::Queued { ahead }).await
    }

    async fn completed(&self, exit_code: i32) -> Result<()> {
        self.publish_terminal(RunEvent::Completed { exit_code }, "completed")
            .await
    }

    async fn failed(&self, message: String) -> Result<()> {
        self.publish_terminal(RunEvent::Failed { message }, "failed")
            .await
    }

    async fn stopped(&self) -> Result<()> {
        self.publish_terminal(RunEvent::Stopped, "stopped").await
    }

    async fn interrupted(&self) -> Result<()> {
        self.publish_terminal(RunEvent::Interrupted, "interrupted")
            .await
    }

    async fn publish_nonterminal(&self, event: RunEvent, stage: &str) -> Result<()> {
        if let Err(err) = self.run.publish(event).await {
            logger::error!(
                "agent run update delivery failed channel={} task={} agent={} run_id={} stage={}: {}",
                self.channel_name,
                self.task_id,
                self.agent_name,
                self.run_id,
                stage,
                err
            );
        }
        Ok(())
    }

    async fn publish_terminal(&self, event: RunEvent, stage: &str) -> Result<()> {
        let result = self.run.publish(event).await;
        if let Err(err) = &result {
            logger::error!(
                "agent run terminal delivery failed channel={} task={} agent={} run_id={} stage={} terminal_delivery_exhausted=true: {}",
                self.channel_name,
                self.task_id,
                self.agent_name,
                self.run_id,
                stage,
                err
            );
        }
        result
    }

    async fn cancelled(&self, cancellation: AgentRunCancellation) -> Result<()> {
        match cancellation {
            AgentRunCancellation::Stopped => self.stopped().await,
            AgentRunCancellation::Interrupted => self.interrupted().await,
        }
    }
}

impl<R> AgentOutput for AgentRunOutput<R>
where
    R: ChannelRun + Send + Sync,
{
    async fn write(&mut self, event: OutputEvent) -> Result<()> {
        self.publish_nonterminal(RunEvent::Output(event), "output")
            .await
    }
}
