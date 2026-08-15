use crate::agent::{
    AgentOutput, AgentRegistry, AgentRunCancellation, AgentRunControl, AgentRunOutcome,
    AgentSessionUpdate, AgentTask, ConfiguredAgent,
};
use crate::channel::{
    Channel, ChannelAgent, ChannelReply, ChannelRun, ChannelRunContext, ChannelTask,
    ConfiguredChannel, InterruptCallback, RunEvent,
};
use crate::config::NodeConfig;
use crate::i18n;
use crate::store::{ChannelSessionKey, SessionKey, SessionStore};
use crate::task::{OutputEvent, TaskContent};
use agora_core::logger;
use anyhow::{Result, bail};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinSet};
use uuid::Uuid;

mod command;
mod execution;

use command::{CommandOutcome, CommandRuntime};
use execution::{ExecutionScheduler, ExecutionScope};

#[cfg(test)]
mod tests;

const CHANNEL_RETRY_DELAY: Duration = Duration::from_secs(1);
const SHUTDOWN_RUN_TIMEOUT: Duration = Duration::from_secs(5);

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
        let agents = self.enabled_agents(channel.name(), task.session_id(), &agents)?;
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
        for agent in agents {
            let agent_task = AgentTask::new(content.clone());
            let isolation_scope = agent.isolation_scope(channel.name(), task.session_id());
            let key = SessionKey::new(agent.name(), isolation_scope.clone());
            let mut execution = self.scheduler.enqueue(ExecutionScope::new(
                channel.name(),
                task.session_id(),
                key.clone(),
            ));
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
            let mut output = AgentRunOutput::new(run);
            let dispatcher = self.clone();
            runs.spawn(async move {
                let mut ahead = execution.ahead();
                while ahead > 0 {
                    tokio::select! {
                        result = output.queued(ahead) => result?,
                        cancellation = control.cancelled() => {
                            drop(execution);
                            return output.cancelled(cancellation).await;
                        }
                    }
                    ahead = tokio::select! {
                        ahead = execution.changed() => ahead?,
                        cancellation = control.cancelled() => {
                            drop(execution);
                            return output.cancelled(cancellation).await;
                        }
                    };
                }
                tokio::select! {
                    result = output.started() => result?,
                    cancellation = control.cancelled() => {
                        drop(execution);
                        return output.cancelled(cancellation).await;
                    }
                }
                let result = dispatcher
                    .execute_agent(&key, &agent, agent_task, control.clone(), &mut output)
                    .await;
                drop(execution);
                match result {
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
                }
            });
        }
        Ok(())
    }

    fn enabled_agents(
        &self,
        channel_name: &str,
        session_id: &str,
        agents: &[ConfiguredAgent],
    ) -> Result<Vec<ConfiguredAgent>> {
        let key = ChannelSessionKey::new(channel_name, session_id);
        let disabled = self
            .store
            .disabled_agents(&key)?
            .into_iter()
            .collect::<HashSet<_>>();
        Ok(agents
            .iter()
            .filter(|agent| !disabled.contains(agent.name()))
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
        key: &SessionKey,
        agent: &ConfiguredAgent,
        task: AgentTask,
        control: AgentRunControl,
        output: &mut O,
    ) -> Result<AgentRunOutcome>
    where
        O: AgentOutput + Send,
    {
        let session_id = self.store.get(key)?;
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
            logger::info!(
                "agent session missing; starting a new session agent={} isolation={}",
                key.agent_name(),
                key.isolation_scope().as_str()
            );
            outcome = match agent.run(task, None, control, output).await? {
                AgentRunOutcome::Completed(outcome) => outcome,
                cancelled @ AgentRunOutcome::Cancelled(_) => return Ok(cancelled),
            };
            if outcome.session_update() == &AgentSessionUpdate::NotFound {
                bail!("agent reported a missing session after starting without a session");
            }
        }

        if let AgentSessionUpdate::Set(session_id) = outcome.session_update() {
            self.store.save(key, session_id)?;
        }
        Ok(AgentRunOutcome::Completed(outcome))
    }
}

pub struct Daemon {
    config: NodeConfig,
    dispatcher: AgentDispatcher,
    commands: Arc<CommandRuntime>,
}

#[derive(Clone)]
pub struct DaemonShutdown {
    scheduler: ExecutionScheduler,
}

impl DaemonShutdown {
    pub async fn interrupt(&self) {
        let interrupted = self.scheduler.interrupt_all();
        if interrupted == 0 {
            return;
        }
        logger::info!("interrupting {} agent runs before shutdown", interrupted);
        if tokio::time::timeout(SHUTDOWN_RUN_TIMEOUT, self.scheduler.wait_until_empty())
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
    pub fn new(mut config: NodeConfig) -> Result<Self> {
        config.validate()?;
        config.apply_proxy_defaults();
        let store = SessionStore::open_default()?;
        let scheduler = ExecutionScheduler::default();
        Ok(Self {
            config,
            dispatcher: AgentDispatcher::from_parts(store.clone(), scheduler.clone()),
            commands: Arc::new(CommandRuntime::new(store, scheduler)?),
        })
    }

    pub fn shutdown_handle(&self) -> DaemonShutdown {
        DaemonShutdown {
            scheduler: self.dispatcher.scheduler.clone(),
        }
    }

    pub async fn run(self) -> Result<()> {
        let Self {
            config,
            dispatcher,
            commands,
        } = self;
        let NodeConfig {
            proxy: _,
            channels,
            agents,
        } = config;
        let agents = AgentRegistry::from_configs(agents)?;
        let shutdown = DaemonShutdown {
            scheduler: dispatcher.scheduler.clone(),
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
            tasks.spawn(async move {
                Self::run_channel(channel, subscribed_agents, dispatcher, commands).await
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
                    Ok(Some(task)) => {
                        let task_channel = channel.clone();
                        let task_agents = agents.clone();
                        let task_dispatcher = dispatcher.clone();
                        let task_commands = Arc::clone(&commands);
                        let predecessor = route_tail.take();
                        let (admitted, successor) = oneshot::channel();
                        route_tail = Some(successor);
                        routes.spawn(async move {
                            if let Some(predecessor) = predecessor {
                                let _ = predecessor.await;
                            }
                            let mut agent_runs = JoinSet::new();
                            let route_result = Self::route_channel_task(
                                &task_channel,
                                &task_agents,
                                &task_dispatcher,
                                &task_commands,
                                task,
                                &mut agent_runs,
                            )
                            .await;
                            let _ = admitted.send(());
                            while let Some(result) = agent_runs.join_next().await {
                                AgentDispatcher::log_run_result(result);
                            }
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
            .handle(channel.name(), task.session_id(), agents, task.input())
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
}

impl<R> AgentRunOutput<R>
where
    R: ChannelRun + Send + Sync,
{
    fn new(run: R) -> Self {
        Self {
            run,
            run_id: Uuid::new_v4().to_string(),
        }
    }

    async fn started(&self) -> Result<()> {
        self.run
            .publish(RunEvent::Started {
                run_id: self.run_id.clone(),
            })
            .await
    }

    async fn queued(&self, ahead: usize) -> Result<()> {
        self.run.publish(RunEvent::Queued { ahead }).await
    }

    async fn completed(&self, exit_code: i32) -> Result<()> {
        self.run.publish(RunEvent::Completed { exit_code }).await
    }

    async fn failed(&self, message: String) -> Result<()> {
        self.run.publish(RunEvent::Failed { message }).await
    }

    async fn stopped(&self) -> Result<()> {
        self.run.publish(RunEvent::Stopped).await
    }

    async fn interrupted(&self) -> Result<()> {
        let result = self.run.publish(RunEvent::Interrupted).await;
        if let Err(err) = &result {
            logger::error!("failed to publish interrupted agent run: {}", err);
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
        self.run.publish(RunEvent::Output(event)).await
    }
}
