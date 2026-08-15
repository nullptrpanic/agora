use agora_node::agent::{
    AgentOutcome, AgentOutput, AgentRunCancellation, AgentRunControl, AgentRunOutcome,
    AgentSessionUpdate, AgentTask, ConfiguredAgent, DeleteSessionOutcome,
};
use agora_node::config::{AgentConfig, AgentSandbox, AgentType, IsolateMode};
use agora_node::task::{OutputEvent, ProgressStatus, TaskAttachment, TaskContent, TokenUsage};
use anyhow::Result;

#[derive(Default)]
struct VecAgentOutput {
    events: Vec<OutputEvent>,
}

impl AgentOutput for VecAgentOutput {
    async fn write(&mut self, event: OutputEvent) -> Result<()> {
        self.events.push(event);
        Ok(())
    }
}

impl VecAgentOutput {
    fn answer_text(&self) -> String {
        self.events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::Answer { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

fn completed(outcome: AgentRunOutcome) -> AgentOutcome {
    let AgentRunOutcome::Completed(outcome) = outcome else {
        panic!("agent run should complete");
    };
    outcome
}

mod codex;
mod control;
mod custom;

fn agent(
    agent_type: AgentType,
    path: impl AsRef<std::path::Path>,
    workspace: impl AsRef<std::path::Path>,
) -> AgentConfig {
    AgentConfig {
        name: "codex-dev".to_string(),
        isolate: IsolateMode::None,
        workspace: workspace.as_ref().to_string_lossy().into_owned(),
        agent_type,
        path: path.as_ref().to_string_lossy().into_owned(),
        model: None,
        effort: None,
        agent_sandbox: None,
        proxy: None,
        timeout_seconds: 3600,
        max_output_bytes: 64 * 1024 * 1024,
        subscribe: Vec::new(),
    }
}

#[test]
fn configured_agent_rejects_unimplemented_one_shot_backends() {
    let workspace = tempfile::tempdir().unwrap();
    for (agent_type, expected) in [
        (AgentType::Coco, "coco agent execution is not implemented"),
        (
            AgentType::ClaudeCode,
            "claude code agent execution is not implemented",
        ),
    ] {
        let result =
            ConfiguredAgent::from_config(agent(agent_type, "/bin/false", workspace.path()));
        let Err(error) = result else {
            panic!("unimplemented agent backend unexpectedly succeeded");
        };
        assert!(error.to_string().contains(expected));
    }
}
