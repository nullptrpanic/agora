use super::*;

#[cfg(unix)]
#[tokio::test]
async fn hardening_relative_executable_is_fixed_before_changing_workdir() {
    use std::os::unix::fs::PermissionsExt;
    let cwd = std::env::current_dir().unwrap();
    let temp = tempfile::tempdir_in(&cwd).unwrap();
    let script = temp.path().join("agent");
    std::fs::write(&script, "#!/bin/sh\nprintf resolved\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let configured = ConfiguredAgent::from_config(agent(
        AgentType::Custom,
        script.strip_prefix(&cwd).unwrap(),
        temp.path().join("different"),
    ))
    .unwrap();
    let mut output = VecAgentOutput::default();
    configured
        .run(
            AgentTask::new(""),
            None,
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap();
    assert_eq!(output.answer_text(), "resolved");
}

#[tokio::test]
async fn custom_agent_streams_raw_command_output() {
    let temp = tempfile::tempdir().unwrap();
    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Custom, "/bin/cat", temp.path())).unwrap();
    let mut output = VecAgentOutput::default();

    let outcome = completed(
        agent
            .run(
                AgentTask::new("hello from custom"),
                None,
                AgentRunControl::new(),
                &mut output,
            )
            .await
            .unwrap(),
    );

    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(outcome.session_update(), &AgentSessionUpdate::Unchanged);
    assert_eq!(output.answer_text(), "hello from custom");
    assert!(temp.path().exists());
}

#[tokio::test]
async fn custom_agent_rejects_attachments_without_a_backend_contract() {
    let temp = tempfile::tempdir().unwrap();
    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Custom, "/bin/cat", temp.path())).unwrap();
    let content = TaskContent::new("analyze this image").with_attachment(TaskAttachment::image(
        "trace.png",
        "image/png",
        b"image-bytes".to_vec(),
    ));
    let mut output = VecAgentOutput::default();

    let error = agent
        .run(
            AgentTask::new(content),
            None,
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "custom agent does not support task attachments"
    );
}

#[tokio::test]
async fn custom_agent_reports_backend_session_deletion_as_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Custom, "/bin/cat", temp.path())).unwrap();

    assert_eq!(
        agent.delete_session("custom-session").await.unwrap(),
        DeleteSessionOutcome::Unsupported
    );
}

#[tokio::test]
async fn custom_agent_applies_the_configured_execution_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = agent(AgentType::Custom, "/bin/sh", temp.path());
    config.timeout_seconds = 1;
    let agent = ConfiguredAgent::from_config(config).unwrap();
    let mut output = VecAgentOutput::default();

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        agent.run(
            AgentTask::new("printf started; exec /bin/sleep 30"),
            None,
            AgentRunControl::new(),
            &mut output,
        ),
    )
    .await
    .expect("configured agent timeout was not applied")
    .unwrap_err();

    assert!(error.to_string().contains("timed out"));
    assert_eq!(output.answer_text(), "started");
}

#[tokio::test]
async fn custom_agent_applies_the_configured_combined_output_limit() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = agent(AgentType::Custom, "/bin/sh", temp.path());
    config.max_output_bytes = 4;
    let agent = ConfiguredAgent::from_config(config).unwrap();
    let mut output = VecAgentOutput::default();

    let error = agent
        .run(
            AgentTask::new("printf 1234; printf 5678 >&2"),
            None,
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("output limit"));
}
