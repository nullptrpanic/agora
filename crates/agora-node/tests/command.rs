use agora_node::agent::command::{Command, CommandOutput};
use anyhow::Result;

#[derive(Default)]
struct RecordingOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    finished: bool,
}

impl CommandOutput for RecordingOutput {
    async fn stdout(&mut self, chunk: &[u8]) -> Result<()> {
        self.stdout.extend_from_slice(chunk);
        Ok(())
    }

    async fn stderr(&mut self, chunk: &[u8]) -> Result<()> {
        self.stderr.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        self.finished = true;
        Ok(())
    }
}

#[tokio::test]
async fn command_handles_process_io_without_agent_protocol_knowledge() {
    let temp = tempfile::tempdir().unwrap();
    let mut output = RecordingOutput::default();
    let command = Command::new("/bin/bash")
        .args([
            "-c",
            "read input; printf 'stdout:%s' \"$input\"; printf 'stderr:done' >&2",
        ])
        .current_dir(temp.path())
        .input("hello from command\n");

    let outcome = command.run(&mut output).await.unwrap();

    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "stdout:hello from command"
    );
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "stderr:done");
    assert!(output.finished);
}

#[tokio::test]
async fn command_injects_configured_environment_variables() {
    let mut output = RecordingOutput::default();
    let command = Command::new("/bin/bash")
        .args(["-c", "printf '%s' \"$AGORA_AGENT_ENV\""])
        .envs([("AGORA_AGENT_ENV", "configured")]);

    let outcome = command.run(&mut output).await.unwrap();

    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "configured");
}
