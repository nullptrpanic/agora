use super::command::{Command, CommandLimits, CommandOutput};
use super::{
    Agent, AgentOutcome, AgentOutput, AgentRequest, AgentSessionUpdate, DeleteSessionOutcome,
};
use crate::config::AgentSandbox;
use crate::task::{OutputEvent, ProgressStatus, TaskAttachment, TaskAttachmentKind, TokenUsage};
use agora_core::logger;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[derive(Clone)]
pub(super) struct CodexAgent {
    name: String,
    path: String,
    model: Option<String>,
    effort: Option<String>,
    agent_sandbox: Option<AgentSandbox>,
    env: HashMap<String, String>,
    limits: CommandLimits,
}

impl CodexAgent {
    pub(super) fn new(
        name: String,
        path: String,
        model: Option<String>,
        effort: Option<String>,
        agent_sandbox: Option<AgentSandbox>,
        env: HashMap<String, String>,
        limits: CommandLimits,
    ) -> Self {
        Self {
            name,
            path,
            model,
            effort,
            agent_sandbox,
            env,
            limits,
        }
    }

    fn command(&self, request: AgentRequest) -> Result<PreparedCommand> {
        let (workdir, content, session_id) = request.into_parts();
        let (input, attachments) = content.into_parts();
        let (attachment_dir, image_paths) = Self::materialize_images(&workdir, &attachments)?;
        let resume_requested = session_id.is_some();
        let mut args = match &session_id {
            Some(_) => vec![
                "exec".to_string(),
                "resume".to_string(),
                "--json".to_string(),
            ],
            None => vec![
                "exec".to_string(),
                "--json".to_string(),
                "--color".to_string(),
                "never".to_string(),
            ],
        };
        args.push("--skip-git-repo-check".to_string());
        self.append_options(&mut args);
        for path in image_paths {
            args.push("--image".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
        if let Some(session_id) = session_id {
            args.push(session_id);
        }
        args.push("-".to_string());
        Ok(PreparedCommand {
            command: Command::new(&self.path)
                .args(args)
                .envs(self.env.clone())
                .current_dir(workdir)
                .input(input)
                .limits(self.limits),
            resume_requested,
            _attachment_dir: attachment_dir,
        })
    }

    fn materialize_images(
        workdir: &Path,
        attachments: &[TaskAttachment],
    ) -> Result<(Option<TempDir>, Vec<PathBuf>)> {
        let images = attachments
            .iter()
            .filter(|attachment| attachment.kind() == TaskAttachmentKind::Image)
            .collect::<Vec<_>>();
        if images.is_empty() {
            return Ok((None, Vec::new()));
        }

        let directory = tempfile::Builder::new()
            .prefix(".agora-attachments-")
            .tempdir_in(workdir)
            .context("create temporary agent attachment directory failed")?;
        let mut paths = Vec::with_capacity(images.len());
        for (index, image) in images.into_iter().enumerate() {
            let extension = Path::new(image.file_name())
                .extension()
                .and_then(|extension| extension.to_str())
                .filter(|extension| {
                    !extension.is_empty()
                        && extension.len() <= 10
                        && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
                })
                .unwrap_or("img");
            let path = directory
                .path()
                .join(format!("image-{}.{}", index + 1, extension));
            std::fs::write(&path, image.data()).with_context(|| {
                format!("write agent image attachment failed: {}", path.display())
            })?;
            paths.push(path);
        }
        Ok((Some(directory), paths))
    }

    fn append_options(&self, args: &mut Vec<String>) {
        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        if let Some(effort) = &self.effort {
            args.push("--config".to_string());
            args.push(format!("model_reasoning_effort={effort}"));
        }
        if let Some(agent_sandbox) = self.agent_sandbox {
            args.push("--config".to_string());
            args.push(format!("sandbox_mode=\"{}\"", agent_sandbox.as_str()));
            args.push("--config".to_string());
            args.push("approval_policy=\"never\"".to_string());
        }
        args.push("--config".to_string());
        args.push("model_reasoning_summary=concise".to_string());
    }
}

impl Agent for CodexAgent {
    async fn run<O>(&self, request: AgentRequest, output: &mut O) -> Result<AgentOutcome>
    where
        O: AgentOutput + Send,
    {
        let PreparedCommand {
            command,
            resume_requested,
            _attachment_dir,
        } = self.command(request)?;
        let mut command_output = CodexCommandOutput::new(&self.name, output, resume_requested);
        let outcome = command.run(&mut command_output).await?;
        let session_update = if command_output.session_not_found() {
            AgentSessionUpdate::NotFound
        } else if let Some(next_session_id) = command_output.take_session_id() {
            logger::info!("agent session observed agent={}", self.name);
            AgentSessionUpdate::Set(next_session_id)
        } else {
            AgentSessionUpdate::Unchanged
        };
        Ok(AgentOutcome::new(outcome.exit_code(), session_update))
    }

    async fn delete_session(&self, session_id: &str) -> Result<DeleteSessionOutcome> {
        let command = Command::new(&self.path)
            .args(["delete", "--force", session_id])
            .envs(self.env.clone())
            .limits(self.limits);
        let mut command_output = DeleteSessionCommandOutput::default();
        let outcome = command.run(&mut command_output).await?;
        if outcome.exit_code() != 0 {
            if missing_session_message(&command_output.message()) {
                return Ok(DeleteSessionOutcome::Deleted);
            }
            bail!(
                "codex delete session failed exit_code={} output={}",
                outcome.exit_code(),
                command_output.message()
            );
        }
        Ok(DeleteSessionOutcome::Deleted)
    }
}

#[derive(Default)]
struct DeleteSessionCommandOutput {
    output: Vec<u8>,
}

impl DeleteSessionCommandOutput {
    fn message(&self) -> String {
        String::from_utf8_lossy(&self.output).trim().to_string()
    }
}

impl CommandOutput for DeleteSessionCommandOutput {
    async fn stdout(&mut self, chunk: &[u8]) -> Result<()> {
        self.output.extend_from_slice(chunk);
        Ok(())
    }

    async fn stderr(&mut self, chunk: &[u8]) -> Result<()> {
        self.output.extend_from_slice(chunk);
        Ok(())
    }
}

struct PreparedCommand {
    command: Command,
    resume_requested: bool,
    _attachment_dir: Option<TempDir>,
}

struct CodexCommandOutput<'a, O> {
    agent_name: &'a str,
    output: &'a mut O,
    stdout_buffer: Vec<u8>,
    stderr_buffer: Vec<u8>,
    session_id: Option<String>,
    pending_message: Option<PendingAgentMessage>,
    resume_requested: bool,
    session_not_found: bool,
}

impl<'a, O> CodexCommandOutput<'a, O>
where
    O: AgentOutput + Send,
{
    fn new(agent_name: &'a str, output: &'a mut O, resume_requested: bool) -> Self {
        Self {
            agent_name,
            output,
            stdout_buffer: Vec::new(),
            stderr_buffer: Vec::new(),
            session_id: None,
            pending_message: None,
            resume_requested,
            session_not_found: false,
        }
    }

    fn take_session_id(&mut self) -> Option<String> {
        self.session_id.take()
    }

    fn session_not_found(&self) -> bool {
        self.session_not_found
    }

    async fn push_stdout(&mut self, chunk: &[u8]) -> Result<()> {
        self.stdout_buffer.extend_from_slice(chunk);
        while let Some(newline) = self.stdout_buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.stdout_buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.handle_line(&String::from_utf8_lossy(&line)).await?;
        }
        Ok(())
    }

    async fn flush_stdout(&mut self) -> Result<()> {
        if self.stdout_buffer.is_empty() {
            return Ok(());
        }
        let line = std::mem::take(&mut self.stdout_buffer);
        self.handle_line(&String::from_utf8_lossy(&line)).await
    }

    async fn flush_stderr(&mut self) -> Result<()> {
        if self.stderr_buffer.is_empty() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&self.stderr_buffer).into_owned();
        if self.resume_requested && missing_session_message(&stderr) {
            self.session_not_found = true;
            return Ok(());
        }
        logger::error!(
            "codex stderr agent={}: {}",
            self.agent_name,
            stderr.trim_end()
        );
        Ok(())
    }

    async fn publish_pending_message(&mut self, final_answer: bool) -> Result<()> {
        let Some(message) = self.pending_message.take() else {
            return Ok(());
        };
        let event = if final_answer {
            OutputEvent::Answer { text: message.text }
        } else {
            OutputEvent::Progress {
                id: message.id,
                text: Self::concise(&message.text, 240),
                status: ProgressStatus::Completed,
            }
        };
        self.output.write(event).await
    }

    async fn handle_item(&mut self, event_type: &str, item: &Value) -> Result<()> {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if item_type == "agent_message" && event_type == "item.completed" {
            self.publish_pending_message(false).await?;
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                self.pending_message = Some(PendingAgentMessage {
                    id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("agent-message")
                        .to_string(),
                    text: text.to_string(),
                });
            }
            return Ok(());
        }

        self.publish_pending_message(false).await?;
        match item_type {
            "reasoning" if event_type == "item.completed" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    let text = Self::concise(text, 600);
                    if !text.is_empty() {
                        self.output.write(OutputEvent::Thinking { text }).await?;
                    }
                }
            }
            "command_execution" => {
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                self.output
                    .write(OutputEvent::CommandExecution {
                        id: Self::progress_id(item),
                        command: command.to_string(),
                        status: Self::progress_status(event_type, item),
                        exit_code: item
                            .get("exit_code")
                            .and_then(Value::as_i64)
                            .and_then(|code| i32::try_from(code).ok()),
                    })
                    .await?;
            }
            "file_change" => {
                let count = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                self.publish_progress(
                    item,
                    format!("Changed {count} file(s)"),
                    Self::progress_status(event_type, item),
                )
                .await?;
            }
            "todo_list" => {
                let items = item
                    .get("items")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let completed = items
                    .iter()
                    .filter(|item| {
                        item.get("completed")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .count();
                self.publish_progress(
                    item,
                    format!("Plan progress: {completed}/{}", items.len()),
                    Self::progress_status(event_type, item),
                )
                .await?;
            }
            "mcp_tool_call" => {
                let server = item.get("server").and_then(Value::as_str).unwrap_or("mcp");
                let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
                self.publish_progress(
                    item,
                    format!("Call `{server}/{tool}`"),
                    Self::progress_status(event_type, item),
                )
                .await?;
            }
            "web_search" => {
                let query = item
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("web search");
                self.publish_progress(
                    item,
                    format!("Search `{}`", Self::concise(&query.replace('`', "'"), 160)),
                    Self::progress_status(event_type, item),
                )
                .await?;
            }
            "error" => {
                let message = item
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex item failed");
                self.publish_progress(item, message.to_string(), ProgressStatus::Failed)
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn publish_progress(
        &mut self,
        item: &Value,
        text: String,
        status: ProgressStatus,
    ) -> Result<()> {
        self.output
            .write(OutputEvent::Progress {
                id: Self::progress_id(item),
                text,
                status,
            })
            .await
    }

    fn progress_id(item: &Value) -> String {
        item.get("id")
            .and_then(Value::as_str)
            .unwrap_or("codex-progress")
            .to_string()
    }

    fn progress_status(event_type: &str, item: &Value) -> ProgressStatus {
        match item.get("status").and_then(Value::as_str) {
            Some("completed") => ProgressStatus::Completed,
            Some("failed" | "declined") => ProgressStatus::Failed,
            Some("in_progress") => ProgressStatus::Running,
            _ if event_type == "item.completed" => ProgressStatus::Completed,
            _ => ProgressStatus::Running,
        }
    }

    fn concise(text: &str, max_chars: usize) -> String {
        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.chars().count() <= max_chars {
            return normalized;
        }
        let mut result = normalized.chars().take(max_chars).collect::<String>();
        result.push_str("...");
        result
    }

    fn token_usage(event: &Value) -> Option<TokenUsage> {
        let usage = event.get("usage")?;
        Some(TokenUsage {
            input_tokens: usage.get("input_tokens")?.as_u64()?,
            cached_input_tokens: usage
                .get("cached_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            output_tokens: usage.get("output_tokens")?.as_u64()?,
            reasoning_output_tokens: usage
                .get("reasoning_output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        })
    }

    async fn handle_line(&mut self, line: &str) -> Result<()> {
        let event = match serde_json::from_str::<Value>(line) {
            Ok(event) => event,
            Err(_) => {
                return self
                    .output
                    .write(OutputEvent::Answer {
                        text: format!("{line}\n"),
                    })
                    .await;
            }
        };

        match event.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                if let Some(thread_id) = event.get("thread_id").and_then(Value::as_str) {
                    self.session_id = Some(thread_id.to_string());
                }
            }
            Some(event_type @ ("item.started" | "item.updated" | "item.completed")) => {
                if let Some(item) = event.get("item") {
                    self.handle_item(event_type, item).await?;
                }
            }
            Some("turn.completed") => {
                self.publish_pending_message(true).await?;
                if let Some(usage) = Self::token_usage(&event) {
                    self.output.write(OutputEvent::Usage(usage)).await?;
                }
            }
            Some("error") | Some("turn.failed") => {
                self.publish_pending_message(false).await?;
                let message = event
                    .get("message")
                    .or_else(|| event.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("codex execution failed");
                self.output
                    .write(OutputEvent::Answer {
                        text: message.to_string(),
                    })
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn missing_session_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no rollout found for thread id")
        || message.contains("session not found")
        || message.contains("thread not found")
}

struct PendingAgentMessage {
    id: String,
    text: String,
}

impl<O> CommandOutput for CodexCommandOutput<'_, O>
where
    O: AgentOutput + Send,
{
    async fn stdout(&mut self, chunk: &[u8]) -> Result<()> {
        self.push_stdout(chunk).await
    }

    async fn stderr(&mut self, chunk: &[u8]) -> Result<()> {
        self.stderr_buffer.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        self.flush_stdout().await?;
        self.publish_pending_message(true).await?;
        self.flush_stderr().await
    }
}
