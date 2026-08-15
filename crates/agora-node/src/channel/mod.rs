use crate::channel::lark::{LarkChannel, LarkRun, LarkTask};
use crate::channel::telegram::{TelegramChannel, TelegramRun, TelegramTask};
use crate::config::ChannelConfig;
use crate::task::{ChannelTaskInput, CommandRequest, OutputEvent};
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub mod lark;
mod permission;
mod telegram;

#[cfg(test)]
pub(crate) mod test_http;

pub trait Channel {
    type Task: ChannelTask;
    type Run: ChannelRun;

    fn name(&self) -> &str;

    fn recv(&mut self) -> impl Future<Output = Result<Option<Self::Task>>> + Send;

    fn open_run(
        &self,
        task: &Self::Task,
        context: ChannelRunContext,
    ) -> impl Future<Output = Result<Self::Run>> + Send;

    fn reply(
        &self,
        task: &Self::Task,
        reply: ChannelReply,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub trait ChannelTask: Clone {
    fn task_id(&self) -> &str;

    fn session_id(&self) -> &str;

    fn input(&self) -> &ChannelTaskInput;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelButtonStyle {
    Default,
    Primary,
    Danger,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelButton {
    text: String,
    style: ChannelButtonStyle,
    command: CommandRequest,
}

impl ChannelButton {
    pub fn new(
        text: impl Into<String>,
        style: ChannelButtonStyle,
        command: CommandRequest,
    ) -> Self {
        Self {
            text: text.into(),
            style,
            command,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn style(&self) -> ChannelButtonStyle {
        self.style
    }

    pub fn command(&self) -> &CommandRequest {
        &self.command
    }
}

#[derive(Clone)]
pub struct InterruptCallback {
    callback: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl InterruptCallback {
    pub(crate) fn new(callback: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }

    pub fn trigger(&self) -> bool {
        (self.callback)()
    }
}

impl fmt::Debug for InterruptCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InterruptCallback")
    }
}

#[derive(Clone, Default)]
pub(crate) struct InterruptCallbacks {
    inner: Arc<Mutex<HashMap<String, InterruptCallback>>>,
}

impl InterruptCallbacks {
    pub(crate) fn register(&self, callback: InterruptCallback) -> InterruptRegistration {
        let id = format!("interrupt-{}", Uuid::new_v4().simple());
        self.callbacks().insert(id.clone(), callback);
        InterruptRegistration {
            id,
            callbacks: self.clone(),
        }
    }

    pub(crate) fn trigger(&self, id: &str) -> bool {
        self.callbacks()
            .remove(id)
            .is_some_and(|callback| callback.trigger())
    }

    fn remove(&self, id: &str) {
        self.callbacks().remove(id);
    }

    fn callbacks(&self) -> std::sync::MutexGuard<'_, HashMap<String, InterruptCallback>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(crate) struct InterruptRegistration {
    id: String,
    callbacks: InterruptCallbacks,
}

impl InterruptRegistration {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for InterruptRegistration {
    fn drop(&mut self) {
        self.callbacks.remove(&self.id);
    }
}

#[derive(Clone, Debug)]
pub struct ChannelRunContext {
    pub agent: ChannelAgent,
    pub interrupt: Option<InterruptCallback>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelAgent {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelAgentStatus {
    name: String,
    enabled: bool,
    button: Option<ChannelButton>,
}

impl ChannelAgentStatus {
    pub fn new(name: impl Into<String>, enabled: bool) -> Self {
        Self {
            name: name.into(),
            enabled,
            button: None,
        }
    }

    pub fn with_button(mut self, button: ChannelButton) -> Self {
        self.button = Some(button);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn button(&self) -> Option<&ChannelButton> {
        self.button.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelReply {
    Text(String),
    AgentList(Vec<ChannelAgentStatus>),
    AgentStatus(ChannelAgentStatus),
}

impl ChannelReply {
    pub fn new(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn agent_list(agents: Vec<ChannelAgentStatus>) -> Self {
        Self::AgentList(agents)
    }

    pub fn agent_status(agent: ChannelAgentStatus) -> Self {
        Self::AgentStatus(agent)
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::AgentList(_) | Self::AgentStatus(_) => None,
        }
    }
}

pub trait ChannelRun: Clone {
    fn publish(&self, event: RunEvent) -> impl Future<Output = Result<()>> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfiguredTask {
    Lark(LarkTask),
    Telegram(TelegramTask),
}

impl ChannelTask for ConfiguredTask {
    fn task_id(&self) -> &str {
        match self {
            ConfiguredTask::Lark(task) => task.task_id(),
            ConfiguredTask::Telegram(task) => task.task_id(),
        }
    }

    fn session_id(&self) -> &str {
        match self {
            ConfiguredTask::Lark(task) => task.session_id(),
            ConfiguredTask::Telegram(task) => task.session_id(),
        }
    }

    fn input(&self) -> &ChannelTaskInput {
        match self {
            ConfiguredTask::Lark(task) => task.input(),
            ConfiguredTask::Telegram(task) => task.input(),
        }
    }
}

#[derive(Clone)]
pub enum ConfiguredRun {
    Lark(LarkRun),
    Telegram(TelegramRun),
}

impl ChannelRun for ConfiguredRun {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        match self {
            ConfiguredRun::Lark(run) => run.publish(event).await,
            ConfiguredRun::Telegram(run) => run.publish(event).await,
        }
    }
}

#[derive(Clone)]
pub enum ConfiguredChannel {
    Lark(LarkChannel),
    Telegram(TelegramChannel),
}

impl ConfiguredChannel {
    pub fn from_config(config: ChannelConfig) -> Result<Option<Self>> {
        match config {
            ChannelConfig::Lark(config) => Ok(Some(Self::Lark(LarkChannel::new(config)?))),
            ChannelConfig::Telegram(config) => {
                Ok(Some(Self::Telegram(TelegramChannel::new(config)?)))
            }
            ChannelConfig::Local(config) => {
                bail!("local channel is not implemented: {}", config.name)
            }
            ChannelConfig::Http(config) => {
                bail!("http channel is not implemented: {}", config.name)
            }
        }
    }
}

impl Channel for ConfiguredChannel {
    type Task = ConfiguredTask;
    type Run = ConfiguredRun;

    fn name(&self) -> &str {
        match self {
            ConfiguredChannel::Lark(channel) => channel.name(),
            ConfiguredChannel::Telegram(channel) => channel.name(),
        }
    }

    async fn recv(&mut self) -> Result<Option<Self::Task>> {
        match self {
            ConfiguredChannel::Lark(channel) => Ok(channel.recv().await?.map(ConfiguredTask::Lark)),
            ConfiguredChannel::Telegram(channel) => {
                Ok(channel.recv().await?.map(ConfiguredTask::Telegram))
            }
        }
    }

    async fn open_run(&self, task: &Self::Task, context: ChannelRunContext) -> Result<Self::Run> {
        match (self, task) {
            (ConfiguredChannel::Lark(channel), ConfiguredTask::Lark(task)) => {
                Ok(ConfiguredRun::Lark(channel.open_run(task, context).await?))
            }
            (ConfiguredChannel::Telegram(channel), ConfiguredTask::Telegram(task)) => Ok(
                ConfiguredRun::Telegram(channel.open_run(task, context).await?),
            ),
            _ => bail!("configured channel and task types do not match"),
        }
    }

    async fn reply(&self, task: &Self::Task, reply: ChannelReply) -> Result<()> {
        match (self, task) {
            (ConfiguredChannel::Lark(channel), ConfiguredTask::Lark(task)) => {
                channel.reply(task, reply).await
            }
            (ConfiguredChannel::Telegram(channel), ConfiguredTask::Telegram(task)) => {
                channel.reply(task, reply).await
            }
            _ => bail!("configured channel and task types do not match"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEvent {
    Queued { ahead: usize },
    Started { run_id: String },
    Output(OutputEvent),
    Completed { exit_code: i32 },
    Failed { message: String },
    Stopped,
    Interrupted,
}

#[cfg(test)]
mod tests;
