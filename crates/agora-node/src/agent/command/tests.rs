use super::{Command, CommandLimits, CommandOutput};
use anyhow::Result;
use std::time::Duration;

#[derive(Default)]
struct CollectedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    finished: bool,
}

impl CommandOutput for CollectedOutput {
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
async fn command_drains_stdout_while_writing_large_stdin() {
    const BYTES: usize = 256 * 1024;
    let mut output = CollectedOutput::default();
    let command = Command::new("/bin/sh")
        .args([
            "-c",
            "/bin/dd if=/dev/zero bs=65536 count=4 2>/dev/null; /bin/cat >/dev/null; printf done",
        ])
        .input("i".repeat(BYTES));

    let outcome = tokio::time::timeout(Duration::from_secs(3), command.run(&mut output))
        .await
        .expect("command deadlocked while stdin and stdout pipes were both full")
        .unwrap();

    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(output.stdout.len(), BYTES + 4);
    assert!(output.stdout.ends_with(b"done"));
    assert!(output.stderr.is_empty());
    assert!(output.finished);
}

#[cfg(unix)]
#[tokio::test]
async fn command_reports_signal_termination_as_128_plus_signal() {
    let mut output = CollectedOutput::default();

    let outcome = Command::new("/bin/sh")
        .args(["-c", "kill -TERM $$"])
        .run(&mut output)
        .await
        .unwrap();

    assert_eq!(outcome.exit_code(), 128 + libc::SIGTERM);
    assert!(output.finished);
}

#[tokio::test]
async fn command_timeout_fails_after_preserving_published_output() {
    let mut output = CollectedOutput::default();
    let command = Command::new("/bin/sh")
        .args(["-c", "printf started; exec /bin/sleep 30"])
        .limits(CommandLimits::new(Duration::from_millis(200), 1024));

    let started = std::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_secs(2), command.run(&mut output))
        .await
        .expect("configured command timeout was not enforced")
        .unwrap_err();

    assert!(error.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(output.stdout, b"started");
    assert!(!output.finished);
}

#[tokio::test]
async fn command_combines_stdout_and_stderr_for_the_output_limit() {
    let mut output = CollectedOutput::default();
    let command = Command::new("/bin/sh")
        .args([
            "-c",
            "printf kept; /bin/sleep 0.1; printf 1234 >&2; /bin/sleep 0.1; printf 5678",
        ])
        .limits(CommandLimits::new(Duration::from_secs(2), 10));

    let error = command.run(&mut output).await.unwrap_err();

    assert!(error.to_string().contains("output limit"));
    assert_eq!(output.stdout, b"kept");
    assert_eq!(output.stderr, b"1234");
    assert!(!output.finished);
}
