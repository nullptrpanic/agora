use super::super::execution::{AdmittedExecution, ExecutionScheduler, ExecutionScope};
use super::super::{AgentDispatcher, Daemon};
use super::{
    AgentDispatch, Argument, CommandContext, CommandExecution, CommandHandler, CommandNode,
    CommandOutcome, CommandRegistry, CommandResolution, CommandRuntime,
};
use crate::agent::{
    AgentOutput, AgentRunCancellation, AgentRunControl, AgentTask, ConfiguredAgent,
};
use crate::channel::{
    Channel, ChannelAgentStatus, ChannelButton, ChannelButtonStyle, ChannelDelivery, ChannelReply,
    ChannelRun, ChannelRunContext, ChannelTask, InterruptCallback, RunEvent,
};
use crate::config::{AgentConfig, AgentType, IsolateMode, IsolationScope};
use crate::store::{SessionKey, SessionStore};
use crate::task::{ChannelTaskInput, CommandRequest, OutputEvent, TaskAttachment, TaskContent};
use anyhow::Result;
use std::collections::VecDeque;
use std::future::pending;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, timeout};

mod ask;
mod channel;
mod execution;
mod registry;
mod reset;

fn run_scope(channel_name: &str, session_id: &str, agent_name: &str) -> ExecutionScope {
    ExecutionScope::new(
        channel_name,
        session_id,
        SessionKey::new(
            agent_name,
            IsolationScope::session(channel_name, session_id),
        ),
        std::path::PathBuf::from("/tmp/agora-command-test"),
    )
}

fn scheduled_run(
    scheduler: &ExecutionScheduler,
    scope: ExecutionScope,
) -> (AdmittedExecution, AgentRunControl) {
    let ticket = scheduler.enqueue(scope);
    let control = ticket.control();
    (ticket, control)
}

fn command_runtime(dispatcher: &AgentDispatcher) -> CommandRuntime {
    CommandRuntime::new(dispatcher.store.clone(), dispatcher.scheduler.clone()).unwrap()
}

fn isolated_command_runtime() -> CommandRuntime {
    let temp = tempfile::tempdir().unwrap();
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("store.db")).unwrap());
    command_runtime(&dispatcher)
}

fn command_test_agent(name: &str, workspace: &std::path::Path) -> ConfiguredAgent {
    ConfiguredAgent::from_config(AgentConfig {
        name: name.to_string(),
        isolate: IsolateMode::Session,
        workspace: workspace.to_string_lossy().into_owned(),
        agent_type: AgentType::Custom,
        path: "/bin/cat".to_string(),
        model: None,
        effort: None,
        agent_sandbox: None,
        proxy: None,
        timeout_seconds: 3600,
        max_output_bytes: 64 * 1024 * 1024,
        subscribe: Vec::new(),
    })
    .unwrap()
}

fn agent_status_with_button(name: &str, enabled: bool) -> ChannelAgentStatus {
    let (text, style, command) = if enabled {
        ("禁用", ChannelButtonStyle::Default, "disable")
    } else {
        ("启用", ChannelButtonStyle::Primary, "enable")
    };
    ChannelAgentStatus::new(name, enabled).with_button(ChannelButton::new(
        text,
        style,
        CommandRequest::new(["ask", command]).with_argument("agent_name", name),
    ))
}

struct IgnoreAgentOutput;

impl AgentOutput for IgnoreAgentOutput {
    async fn write(&mut self, _event: OutputEvent) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct CommandTestTask {
    task_id: String,
    session_id: String,
    input: ChannelTaskInput,
}

impl CommandTestTask {
    fn new(task_id: &str, session_id: &str, content: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            input: ChannelTaskInput::Message(TaskContent::new(content)),
        }
    }

    fn agent_enabled_action(
        task_id: &str,
        session_id: &str,
        agent_name: &str,
        enabled: bool,
    ) -> Self {
        Self {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            input: ChannelTaskInput::Command(
                CommandRequest::new(["ask", if enabled { "enable" } else { "disable" }])
                    .with_argument("agent_name", agent_name),
            ),
        }
    }
}

impl ChannelTask for CommandTestTask {
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

#[derive(Clone)]
struct CommandTestRun {
    events: Arc<Mutex<Vec<RunEvent>>>,
}

impl ChannelRun for CommandTestRun {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

#[derive(Clone)]
struct CommandTestChannel {
    tasks: VecDeque<CommandTestTask>,
    received: Option<Arc<AtomicUsize>>,
    events: Arc<Mutex<Vec<RunEvent>>>,
    contexts: Arc<Mutex<Vec<String>>>,
    interrupts: Arc<Mutex<Vec<InterruptCallback>>>,
    replies: Arc<Mutex<Vec<ChannelReply>>>,
}

impl CommandTestChannel {
    fn new(replies: Arc<Mutex<Vec<ChannelReply>>>) -> Self {
        Self {
            tasks: VecDeque::new(),
            received: None,
            events: Arc::new(Mutex::new(Vec::new())),
            contexts: Arc::new(Mutex::new(Vec::new())),
            interrupts: Arc::new(Mutex::new(Vec::new())),
            replies,
        }
    }
}

impl Channel for CommandTestChannel {
    type Task = CommandTestTask;
    type Run = CommandTestRun;

    fn name(&self) -> &str {
        "lark"
    }

    async fn recv(&mut self) -> Result<Option<ChannelDelivery<Self::Task>>> {
        if let Some(task) = self.tasks.pop_front() {
            if let Some(received) = &self.received {
                received.fetch_add(1, Ordering::Release);
            }
            Ok(Some(ChannelDelivery::untracked(task)))
        } else {
            pending().await
        }
    }

    async fn open_run(&self, _task: &Self::Task, context: ChannelRunContext) -> Result<Self::Run> {
        self.contexts.lock().unwrap().push(context.agent.name);
        if let Some(interrupt) = context.interrupt {
            self.interrupts.lock().unwrap().push(interrupt);
        }
        Ok(CommandTestRun {
            events: Arc::clone(&self.events),
        })
    }

    async fn reply(&self, _task: &Self::Task, reply: ChannelReply) -> Result<()> {
        self.replies.lock().unwrap().push(reply);
        Ok(())
    }
}
