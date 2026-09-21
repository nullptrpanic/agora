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

// Run in a separate test process so setsid never touches the test runner itself.
#[cfg(unix)]
#[test]
fn detached_pipe_holder_fixture() {
    use std::os::unix::process::CommandExt;

    let Some(pid_file) = std::env::var_os("AGORA_TEST_DETACHED_PID_FILE") else {
        return;
    };
    let mut command = std::process::Command::new("/bin/sleep");
    command.arg("30");
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    std::fs::write(pid_file, child.id().to_string()).unwrap();
    println!("parent finished");
    eprintln!("diagnostic retained");
    // Reap if this fixture is ever changed to leave the child already exited.
    let _ = child.try_wait();
}

#[cfg(unix)]
#[tokio::test]
async fn command_finishes_when_detached_descendant_keeps_output_pipes_open() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("child.pid");
    let command = Command::new(std::env::current_exe().unwrap().to_str().unwrap())
        .args([
            "--exact",
            "agent::command::tests::detached_pipe_holder_fixture",
            "--nocapture",
        ])
        .envs([("AGORA_TEST_DETACHED_PID_FILE", pid_file.to_str().unwrap())]);
    let mut output = CollectedOutput::default();
    let outcome = tokio::time::timeout(Duration::from_secs(5), command.run(&mut output)).await;
    if let Ok(pid) = std::fs::read_to_string(pid_file) {
        unsafe { libc::kill(pid.parse().unwrap(), libc::SIGKILL) };
    }
    let outcome = outcome
        .expect("exited parent still waited for an escaped child's pipes")
        .unwrap();
    assert_eq!(outcome.exit_code(), 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains("parent finished"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("diagnostic retained"));
    assert!(output.finished);
}

#[tokio::test]
async fn command_drain_budget_excludes_slow_output_delivery() {
    #[derive(Default)]
    struct SlowOutput(Vec<u8>);
    impl CommandOutput for SlowOutput {
        async fn stdout(&mut self, chunk: &[u8]) -> Result<()> {
            self.0.extend_from_slice(chunk);
            tokio::time::sleep(Duration::from_millis(1100)).await;
            Ok(())
        }
        async fn stderr(&mut self, _: &[u8]) -> Result<()> {
            Ok(())
        }
    }
    let mut output = SlowOutput::default();
    let command = Command::new("/bin/sh").args([
        "-c",
        "/bin/dd if=/dev/zero bs=4096 count=2 2>/dev/null; printf tail",
    ]);
    let outcome = tokio::time::timeout(Duration::from_secs(8), command.run(&mut output))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(
        output.0.len(),
        8196,
        "slow image delivery discarded buffered stdout"
    );
    assert!(output.0.ends_with(b"tail"));
}
