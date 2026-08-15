use super::*;

#[cfg(unix)]
fn process_test_timeout() -> std::time::Duration {
    if std::env::var_os("CARGO_LLVM_COV").is_some() {
        std::time::Duration::from_secs(60)
    } else {
        std::time::Duration::from_secs(10)
    }
}

#[cfg(unix)]
struct StartSignalOutput {
    started: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(unix)]
impl AgentOutput for StartSignalOutput {
    async fn write(&mut self, _event: OutputEvent) -> Result<()> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        Ok(())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn configured_agent_run_owns_its_cancellation() {
    use std::os::unix::fs::PermissionsExt;

    let test_timeout = process_test_timeout();

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("slow-agent");
    std::fs::write(&script, "#!/bin/sh\nprintf 'started\\n'\nexec sleep 30\n").unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Custom, &script, temp.path())).unwrap();
    let control = AgentRunControl::new();
    let stop = control.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let mut run = tokio::spawn(async move {
        let mut output = StartSignalOutput {
            started: Some(started_tx),
        };
        agent
            .run(AgentTask::new("long task"), None, control, &mut output)
            .await
    });

    tokio::select! {
        started = tokio::time::timeout(test_timeout, started_rx) => {
            started
                .expect("agent command did not produce startup output before the timeout")
                .expect("agent command stopped before producing startup output");
        }
        result = &mut run => {
            panic!("agent run finished before startup output: {result:?}");
        }
    }
    assert!(stop.stop());

    assert_eq!(
        tokio::time::timeout(test_timeout, run)
            .await
            .expect("agent run did not stop before the timeout")
            .expect("agent run task failed")
            .expect("agent run returned an error"),
        AgentRunOutcome::Cancelled(AgentRunCancellation::Stopped)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn configured_agent_cancellation_stops_descendant_processes() {
    use std::os::unix::fs::PermissionsExt;

    let test_timeout = process_test_timeout();

    let temp = tempfile::tempdir().unwrap();
    let descendant_pid = temp.path().join("descendant.pid");
    let script = temp.path().join("agent-with-descendant");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > '{}'\nprintf 'started\\n'\nwait\n",
            descendant_pid.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Custom, &script, temp.path())).unwrap();
    let control = AgentRunControl::new();
    let stop = control.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let run = tokio::spawn(async move {
        let mut output = StartSignalOutput {
            started: Some(started_tx),
        };
        agent
            .run(AgentTask::new("long task"), None, control, &mut output)
            .await
    });

    tokio::time::timeout(test_timeout, started_rx)
        .await
        .expect("agent command did not produce startup output before the timeout")
        .expect("agent command stopped before producing startup output");
    let descendant_pid = std::fs::read_to_string(&descendant_pid)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(stop.stop());
    assert_eq!(
        tokio::time::timeout(test_timeout, run)
            .await
            .expect("agent run did not stop before the timeout")
            .expect("agent run task failed")
            .expect("agent run returned an error"),
        AgentRunOutcome::Cancelled(AgentRunCancellation::Stopped)
    );

    let descendant_stopped = tokio::time::timeout(test_timeout, async {
        loop {
            if unsafe { libc::kill(descendant_pid as libc::pid_t, 0) } == -1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    if !descendant_stopped {
        unsafe { libc::kill(descendant_pid as libc::pid_t, libc::SIGKILL) };
    }
    assert!(descendant_stopped, "descendant process remained alive");
}

#[cfg(unix)]
#[tokio::test]
async fn configured_agent_does_not_wait_for_descendant_pipe_eof() {
    use std::os::unix::fs::PermissionsExt;

    let test_timeout = process_test_timeout();

    let temp = tempfile::tempdir().unwrap();
    let descendant_pid = temp.path().join("descendant.pid");
    let script = temp.path().join("agent-with-detached-descendant");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > '{}'\nprintf 'done\\n'\nexit 0\n",
            descendant_pid.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Custom, &script, temp.path())).unwrap();
    let mut output = VecAgentOutput::default();
    let run = agent.run(
        AgentTask::new("short task"),
        None,
        AgentRunControl::new(),
        &mut output,
    );

    let outcome = tokio::time::timeout(test_timeout, run).await;
    if outcome.is_err()
        && let Ok(descendant_pid) = std::fs::read_to_string(&descendant_pid)
        && let Ok(descendant_pid) = descendant_pid.trim().parse::<libc::pid_t>()
    {
        unsafe { libc::kill(descendant_pid, libc::SIGKILL) };
    }
    let outcome = outcome
        .expect("agent waited for a descendant to close inherited output pipes")
        .unwrap();

    assert!(matches!(outcome, AgentRunOutcome::Completed(_)));
    assert_eq!(output.answer_text(), "done\n");
}

#[tokio::test]
async fn agent_run_control_keeps_the_first_cancellation_reason() {
    let control = AgentRunControl::new();

    assert!(control.stop());
    assert!(!control.interrupt());
    assert_eq!(control.cancelled().await, AgentRunCancellation::Stopped);
}

#[tokio::test]
async fn agent_run_control_defaults_to_running_and_can_be_interrupted() {
    let control = AgentRunControl::default();

    assert_eq!(format!("{control:?}"), "AgentRunControl { state: Running }");
    assert!(control.interrupt());
    assert!(!control.stop());
    assert_eq!(
        format!("{control:?}"),
        "AgentRunControl { state: Cancelled(Interrupted) }"
    );
    assert_eq!(control.cancelled().await, AgentRunCancellation::Interrupted);
}
