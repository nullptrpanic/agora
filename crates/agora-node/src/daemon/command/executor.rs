use super::CommandArguments;
use crate::agent::ConfiguredAgent;
use crate::channel::ChannelReply;
use crate::task::TaskContent;
use anyhow::Result;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type CommandFuture = Pin<Box<dyn Future<Output = Result<CommandExecution>> + Send + 'static>>;

pub(in crate::daemon) struct AgentDispatch {
    agents: Vec<ConfiguredAgent>,
    content: TaskContent,
}

impl AgentDispatch {
    pub(in crate::daemon) fn new(agents: Vec<ConfiguredAgent>, content: TaskContent) -> Self {
        Self { agents, content }
    }

    pub(in crate::daemon) fn into_parts(self) -> (Vec<ConfiguredAgent>, TaskContent) {
        (self.agents, self.content)
    }
}

pub(in crate::daemon) enum CommandExecution {
    Reply(Option<ChannelReply>),
    Dispatch(AgentDispatch),
}

impl From<Option<ChannelReply>> for CommandExecution {
    fn from(reply: Option<ChannelReply>) -> Self {
        Self::Reply(reply)
    }
}

impl From<AgentDispatch> for CommandExecution {
    fn from(dispatch: AgentDispatch) -> Self {
        Self::Dispatch(dispatch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandSource {
    Text,
    Structured,
}

pub(in crate::daemon) struct CommandContext {
    channel_name: String,
    session_id: String,
    agents: Vec<ConfiguredAgent>,
    source: CommandSource,
    message: Option<TaskContent>,
}

impl CommandContext {
    pub(in crate::daemon) fn text(
        channel_name: impl Into<String>,
        session_id: impl Into<String>,
        agents: Vec<ConfiguredAgent>,
    ) -> Self {
        Self::new(channel_name, session_id, agents, CommandSource::Text)
    }

    pub(in crate::daemon) fn structured(
        channel_name: impl Into<String>,
        session_id: impl Into<String>,
        agents: Vec<ConfiguredAgent>,
    ) -> Self {
        Self::new(channel_name, session_id, agents, CommandSource::Structured)
    }

    fn new(
        channel_name: impl Into<String>,
        session_id: impl Into<String>,
        agents: Vec<ConfiguredAgent>,
        source: CommandSource,
    ) -> Self {
        Self {
            channel_name: channel_name.into(),
            session_id: session_id.into(),
            agents,
            source,
            message: None,
        }
    }

    pub(super) fn with_message(mut self, message: TaskContent) -> Self {
        self.message = Some(message);
        self
    }

    pub(super) fn channel_name(&self) -> &str {
        &self.channel_name
    }

    pub(super) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(super) fn agents(&self) -> &[ConfiguredAgent] {
        &self.agents
    }

    pub(super) fn is_structured(&self) -> bool {
        self.source == CommandSource::Structured
    }

    pub(super) fn message(&self) -> Option<&TaskContent> {
        self.message.as_ref()
    }
}

type HandlerFn = dyn Fn(CommandContext, CommandArguments) -> CommandFuture + Send + Sync + 'static;

#[derive(Clone)]
pub(in crate::daemon) struct CommandHandler {
    handler: Arc<HandlerFn>,
}

impl CommandHandler {
    pub(in crate::daemon) fn new<F, Fut, Output>(handler: F) -> Self
    where
        F: Fn(CommandContext, CommandArguments) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output>> + Send + 'static,
        Output: Into<CommandExecution> + Send + 'static,
    {
        Self {
            handler: Arc::new(move |context, arguments| {
                let future = handler(context, arguments);
                Box::pin(async move { future.await.map(Into::into) })
            }),
        }
    }

    pub(in crate::daemon) async fn execute(
        &self,
        context: CommandContext,
        arguments: CommandArguments,
    ) -> Result<CommandExecution> {
        (self.handler)(context, arguments).await
    }
}

impl fmt::Debug for CommandHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CommandHandler")
    }
}

impl PartialEq for CommandHandler {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.handler, &other.handler)
    }
}

impl Eq for CommandHandler {}
