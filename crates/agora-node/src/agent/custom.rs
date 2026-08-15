use super::command::{Command, CommandLimits, CommandOutput};
use super::{
    Agent, AgentOutcome, AgentOutput, AgentRequest, AgentSessionUpdate, DeleteSessionOutcome,
};
use crate::task::OutputEvent;
use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Clone)]
pub(super) struct CustomAgent {
    path: String,
    env: HashMap<String, String>,
    limits: CommandLimits,
}

impl CustomAgent {
    pub(super) fn new(path: String, env: HashMap<String, String>, limits: CommandLimits) -> Self {
        Self { path, env, limits }
    }
}

impl Agent for CustomAgent {
    async fn run<O>(&self, request: AgentRequest, output: &mut O) -> Result<AgentOutcome>
    where
        O: AgentOutput + Send,
    {
        let (workdir, content, _) = request.into_parts();
        let (input, attachments) = content.into_parts();
        if !attachments.is_empty() {
            bail!("custom agent does not support task attachments");
        }
        let command = Command::new(&self.path)
            .envs(self.env.clone())
            .current_dir(workdir)
            .input(input)
            .limits(self.limits);
        let mut command_output = RawCommandOutput::new(output);
        let outcome = command.run(&mut command_output).await?;
        Ok(AgentOutcome::new(
            outcome.exit_code(),
            AgentSessionUpdate::Unchanged,
        ))
    }

    async fn delete_session(&self, _session_id: &str) -> Result<DeleteSessionOutcome> {
        Ok(DeleteSessionOutcome::Unsupported)
    }
}

struct RawCommandOutput<'a, O> {
    output: &'a mut O,
    stdout_buffer: Vec<u8>,
    stderr_buffer: Vec<u8>,
}

impl<'a, O> RawCommandOutput<'a, O> {
    fn new(output: &'a mut O) -> Self {
        Self {
            output,
            stdout_buffer: Vec::new(),
            stderr_buffer: Vec::new(),
        }
    }

    async fn write_answer(&mut self, text: String) -> Result<()>
    where
        O: AgentOutput + Send,
    {
        if text.is_empty() {
            return Ok(());
        }
        self.output.write(OutputEvent::Answer { text }).await
    }

    async fn flush(&mut self) -> Result<()>
    where
        O: AgentOutput + Send,
    {
        let stdout = decode_utf8(&mut self.stdout_buffer, &[], true);
        self.write_answer(stdout).await?;
        let stderr = decode_utf8(&mut self.stderr_buffer, &[], true);
        self.write_answer(stderr).await
    }
}

fn decode_utf8(buffer: &mut Vec<u8>, chunk: &[u8], final_chunk: bool) -> String {
    buffer.extend_from_slice(chunk);
    let mut decoded = String::new();
    loop {
        match std::str::from_utf8(buffer) {
            Ok(text) => {
                decoded.push_str(text);
                buffer.clear();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                decoded.push_str(
                    std::str::from_utf8(&buffer[..valid])
                        .expect("UTF-8 validator reported an invalid valid prefix"),
                );
                if let Some(invalid) = error.error_len() {
                    decoded.push('�');
                    buffer.drain(..valid + invalid);
                } else {
                    buffer.drain(..valid);
                    if final_chunk {
                        decoded.push_str(&String::from_utf8_lossy(buffer));
                        buffer.clear();
                    }
                    break;
                }
            }
        }
    }
    decoded
}

impl<O> CommandOutput for RawCommandOutput<'_, O>
where
    O: AgentOutput + Send,
{
    async fn stdout(&mut self, chunk: &[u8]) -> Result<()> {
        let text = decode_utf8(&mut self.stdout_buffer, chunk, false);
        self.write_answer(text).await
    }

    async fn stderr(&mut self, chunk: &[u8]) -> Result<()> {
        let text = decode_utf8(&mut self.stderr_buffer, chunk, false);
        self.write_answer(text).await
    }

    async fn finish(&mut self) -> Result<()> {
        self.flush().await
    }
}

#[cfg(test)]
mod tests;
