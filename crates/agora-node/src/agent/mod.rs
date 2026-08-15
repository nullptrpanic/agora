use crate::config::{AgentConfig, AgentType, HttpProxy, IsolationScope};
use crate::task::{OutputEvent, TaskContent};
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

pub mod command;

mod codex;
mod custom;
mod run;

use codex::CodexAgent;
use command::CommandLimits;
use custom::CustomAgent;
pub use run::{AgentRunCancellation, AgentRunControl, AgentRunOutcome};

pub trait AgentOutput {
    fn write(&mut self, event: OutputEvent) -> impl Future<Output = Result<()>> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTask {
    content: TaskContent,
}

impl AgentTask {
    pub fn new(content: impl Into<TaskContent>) -> Self {
        Self {
            content: content.into(),
        }
    }

    fn into_request(
        self,
        config: &AgentConfig,
        agent_session_id: Option<String>,
    ) -> Result<AgentRequest> {
        let workdir = config.workdir();
        std::fs::create_dir_all(&workdir)?;
        Ok(AgentRequest {
            workdir,
            content: self.content,
            session_id: agent_session_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRequest {
    workdir: PathBuf,
    content: TaskContent,
    session_id: Option<String>,
}

impl AgentRequest {
    pub(crate) fn into_parts(self) -> (PathBuf, TaskContent, Option<String>) {
        (self.workdir, self.content, self.session_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentSessionUpdate {
    Unchanged,
    Set(String),
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteSessionOutcome {
    Deleted,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentOutcome {
    exit_code: i32,
    session_update: AgentSessionUpdate,
}

impl AgentOutcome {
    pub(crate) fn new(exit_code: i32, session_update: AgentSessionUpdate) -> Self {
        Self {
            exit_code,
            session_update,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn session_update(&self) -> &AgentSessionUpdate {
        &self.session_update
    }
}

pub trait Agent {
    fn run<O>(
        &self,
        request: AgentRequest,
        output: &mut O,
    ) -> impl Future<Output = Result<AgentOutcome>> + Send
    where
        O: AgentOutput + Send;

    fn delete_session(
        &self,
        session_id: &str,
    ) -> impl Future<Output = Result<DeleteSessionOutcome>> + Send;
}

#[derive(Clone)]
enum AgentBackend {
    Codex(CodexAgent),
    Custom(CustomAgent),
}

impl Agent for AgentBackend {
    async fn run<O>(&self, request: AgentRequest, output: &mut O) -> Result<AgentOutcome>
    where
        O: AgentOutput + Send,
    {
        match self {
            AgentBackend::Codex(agent) => agent.run(request, output).await,
            AgentBackend::Custom(agent) => agent.run(request, output).await,
        }
    }

    async fn delete_session(&self, session_id: &str) -> Result<DeleteSessionOutcome> {
        match self {
            AgentBackend::Codex(agent) => agent.delete_session(session_id).await,
            AgentBackend::Custom(agent) => agent.delete_session(session_id).await,
        }
    }
}

#[derive(Clone)]
pub struct ConfiguredAgent {
    config: AgentConfig,
    backend: AgentBackend,
}

impl ConfiguredAgent {
    pub fn from_config(config: AgentConfig) -> Result<Self> {
        let env = Self::proxy_environment(config.proxy.as_ref());
        let limits = CommandLimits::new(
            std::time::Duration::from_secs(config.timeout_seconds),
            config.max_output_bytes,
        );
        let backend = match config.agent_type {
            AgentType::Codex => AgentBackend::Codex(CodexAgent::new(
                config.name.clone(),
                config.path.clone(),
                config.model.clone(),
                config.effort.clone(),
                config.agent_sandbox,
                env,
                limits,
            )),
            AgentType::Custom => {
                AgentBackend::Custom(CustomAgent::new(config.path.clone(), env, limits))
            }
            AgentType::Coco => {
                return Err(anyhow!("one-shot coco agent execution is not implemented"));
            }
            AgentType::ClaudeCode => {
                return Err(anyhow!(
                    "one-shot claude code agent execution is not implemented"
                ));
            }
        };
        Ok(Self { config, backend })
    }

    fn proxy_environment(proxy: Option<&HttpProxy>) -> HashMap<String, String> {
        let Some(proxy) = proxy else {
            return HashMap::new();
        };
        let value = proxy.environment_value();
        ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"]
            .into_iter()
            .map(|name| (name.to_string(), value.clone()))
            .collect()
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn subscribes_to(&self, channel_name: &str) -> bool {
        self.config
            .subscribe
            .iter()
            .any(|subscription| subscription.channel == channel_name)
    }

    pub fn isolation_scope(&self, channel_name: &str, session_id: &str) -> IsolationScope {
        self.config.isolation_scope(channel_name, session_id)
    }

    pub async fn run<O>(
        &self,
        task: AgentTask,
        session_id: Option<String>,
        control: AgentRunControl,
        output: &mut O,
    ) -> Result<AgentRunOutcome>
    where
        O: AgentOutput + Send,
    {
        let request = task.into_request(&self.config, session_id)?;
        tokio::select! {
            biased;
            cancellation = control.cancelled() => {
                Ok(AgentRunOutcome::Cancelled(cancellation))
            }
            outcome = self.backend.run(request, output) => {
                outcome.map(AgentRunOutcome::Completed)
            }
        }
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<DeleteSessionOutcome> {
        self.backend.delete_session(session_id).await
    }
}

pub struct AgentRegistry {
    agents: Vec<ConfiguredAgent>,
}

impl AgentRegistry {
    pub fn from_configs(configs: Vec<AgentConfig>) -> Result<Self> {
        let agents = configs
            .into_iter()
            .map(ConfiguredAgent::from_config)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { agents })
    }

    pub fn subscribed_to(&self, channel_name: &str) -> Vec<ConfiguredAgent> {
        self.agents
            .iter()
            .filter(|agent| agent.subscribes_to(channel_name))
            .cloned()
            .collect()
    }
}
