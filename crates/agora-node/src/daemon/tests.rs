use super::{AgentDispatcher, AgentRunOutput, CommandRuntime, Daemon, DaemonShutdown};
use crate::agent::{AgentRegistry, ConfiguredAgent};
use crate::channel::{
    Channel, ChannelDelivery, ChannelReply, ChannelRun, ChannelRunContext, ChannelTask,
    ConfiguredChannel, RunEvent,
};
use crate::config::{
    AgentConfig, AgentSubscription, AgentType, ChannelConfig, IsolateMode, IsolationScope,
    LarkChannelConfig, NamedChannelConfig, NodeConfig,
};
use crate::store::{SessionKey, SessionStore};
use crate::task::{ChannelTaskInput, OutputEvent, TaskContent};
use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;

impl AgentDispatcher {
    pub(super) fn new(store: SessionStore) -> Self {
        Self::from_parts(store, Default::default())
    }

    async fn dispatch_channel_task<C>(
        &self,
        channel: &C,
        agents: Vec<ConfiguredAgent>,
        task: C::Task,
    ) -> Result<()>
    where
        C: Channel + Sync,
        C::Task: Send + Sync + 'static,
        C::Run: Send + Sync + 'static,
    {
        let mut runs = JoinSet::new();
        self.start_channel_task(channel, agents, task, &mut runs)
            .await?;

        while let Some(result) = runs.join_next().await {
            result??;
        }
        Ok(())
    }
}

mod dispatcher;
mod queue;
mod reliability;
mod sessions;

fn agent(name: &str, channel: &str) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        isolate: IsolateMode::None,
        workspace: "/tmp/agora-agent".to_string(),
        agent_type: AgentType::Codex,
        path: "/bin/cat".to_string(),
        model: None,
        effort: None,
        agent_sandbox: None,
        proxy: None,
        timeout_seconds: 3600,
        max_output_bytes: 64 * 1024 * 1024,
        subscribe: vec![AgentSubscription {
            channel: channel.to_string(),
            filter: None,
        }],
    }
}

fn custom_agent(name: &str) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        isolate: IsolateMode::None,
        workspace: "/tmp/agora-agent".to_string(),
        agent_type: AgentType::Custom,
        path: "/bin/cat".to_string(),
        model: None,
        effort: None,
        agent_sandbox: None,
        proxy: None,
        timeout_seconds: 3600,
        max_output_bytes: 64 * 1024 * 1024,
        subscribe: Vec::new(),
    }
}

#[derive(Clone)]
struct TestTask;

impl ChannelTask for TestTask {
    fn task_id(&self) -> &str {
        "task-1"
    }

    fn session_id(&self) -> &str {
        "session-1"
    }

    fn input(&self) -> &ChannelTaskInput {
        static INPUT: std::sync::OnceLock<ChannelTaskInput> = std::sync::OnceLock::new();
        INPUT.get_or_init(|| ChannelTaskInput::Message(TaskContent::new("hello")))
    }
}

#[derive(Clone)]
struct RecordingRun {
    events: Arc<Mutex<Vec<RunEvent>>>,
}

impl ChannelRun for RecordingRun {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

struct RecordingChannel {
    contexts: Arc<Mutex<Vec<ChannelRunContext>>>,
    events: Arc<Mutex<Vec<RunEvent>>>,
}

#[derive(Clone)]
struct ReplyTask {
    input: ChannelTaskInput,
}

impl ChannelTask for ReplyTask {
    fn task_id(&self) -> &str {
        "reply-task"
    }

    fn session_id(&self) -> &str {
        "reply-session"
    }

    fn input(&self) -> &ChannelTaskInput {
        &self.input
    }
}

struct ReplyChannel {
    replies: Arc<Mutex<Vec<ChannelReply>>>,
}

impl Channel for ReplyChannel {
    type Task = ReplyTask;
    type Run = RecordingRun;

    fn name(&self) -> &str {
        "reply"
    }

    async fn recv(&mut self) -> Result<Option<ChannelDelivery<Self::Task>>> {
        Ok(None)
    }

    async fn open_run(&self, _task: &Self::Task, _context: ChannelRunContext) -> Result<Self::Run> {
        Ok(RecordingRun {
            events: Arc::new(Mutex::new(Vec::new())),
        })
    }

    async fn reply(&self, _task: &Self::Task, reply: ChannelReply) -> Result<()> {
        self.replies.lock().unwrap().push(reply);
        Ok(())
    }
}

#[derive(Clone)]
struct FailingRun;

impl ChannelRun for FailingRun {
    async fn publish(&self, _event: RunEvent) -> Result<()> {
        anyhow::bail!("publish failed")
    }
}

impl Channel for RecordingChannel {
    type Task = TestTask;
    type Run = RecordingRun;

    fn name(&self) -> &str {
        "test"
    }

    async fn recv(&mut self) -> Result<Option<ChannelDelivery<Self::Task>>> {
        Ok(None)
    }

    async fn open_run(&self, _task: &Self::Task, context: ChannelRunContext) -> Result<Self::Run> {
        self.contexts.lock().unwrap().push(context);
        Ok(RecordingRun {
            events: Arc::clone(&self.events),
        })
    }

    async fn reply(&self, _task: &Self::Task, _reply: ChannelReply) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct ScopedTask {
    task_id: String,
    session_id: String,
    input: ChannelTaskInput,
}

impl ScopedTask {
    fn new(task_id: &str, session_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            input: ChannelTaskInput::Message(TaskContent::new("hello")),
        }
    }
}

impl ChannelTask for ScopedTask {
    fn task_id(&self) -> &str {
        &self.task_id
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn input(&self) -> &ChannelTaskInput {
        &self.input
    }
}

struct ScopedChannel {
    name: String,
    events: Arc<Mutex<Vec<RunEvent>>>,
}

impl ScopedChannel {
    fn new(name: &str) -> Self {
        Self::with_events(name, Arc::new(Mutex::new(Vec::new())))
    }

    fn with_events(name: &str, events: Arc<Mutex<Vec<RunEvent>>>) -> Self {
        Self {
            name: name.to_string(),
            events,
        }
    }
}

impl Channel for ScopedChannel {
    type Task = ScopedTask;
    type Run = RecordingRun;

    fn name(&self) -> &str {
        &self.name
    }

    async fn recv(&mut self) -> Result<Option<ChannelDelivery<Self::Task>>> {
        Ok(None)
    }

    async fn open_run(&self, _task: &Self::Task, _context: ChannelRunContext) -> Result<Self::Run> {
        Ok(RecordingRun {
            events: Arc::clone(&self.events),
        })
    }

    async fn reply(&self, _task: &Self::Task, _reply: ChannelReply) -> Result<()> {
        Ok(())
    }
}
