use super::*;

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_uses_the_session_supplied_by_its_caller() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"$*\" >> invocations\n",
            "printf '%s\\n' \"$HTTP_PROXY\" \"$HTTPS_PROXY\" \"$http_proxy\" \"$https_proxy\" > configured-proxy\n",
            "cat >/dev/null\n",
            "printf '%s\\n' ",
            "'{\"type\":\"thread.started\",\"thread_id\":\"thread-123\"}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"hello from codex\"}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let mut config = agent(AgentType::Codex, &script, temp.path());
    config.model = Some("gpt-5.4".to_string());
    config.effort = Some("xhigh".to_string());
    config.agent_sandbox = Some(AgentSandbox::DangerFullAccess);
    config.proxy = Some("user:password@127.0.0.1:7890".parse().unwrap());
    let agent = ConfiguredAgent::from_config(config).unwrap();
    let mut first_output = VecAgentOutput::default();
    let mut second_output = VecAgentOutput::default();

    let first_outcome = completed(
        agent
            .run(
                AgentTask::new("first"),
                None,
                AgentRunControl::new(),
                &mut first_output,
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        first_outcome.session_update(),
        &AgentSessionUpdate::Set("thread-123".to_string())
    );

    let second_outcome = completed(
        agent
            .run(
                AgentTask::new("second"),
                Some("thread-123".to_string()),
                AgentRunControl::new(),
                &mut second_output,
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        second_outcome.session_update(),
        &AgentSessionUpdate::Set("thread-123".to_string())
    );

    let invocations = std::fs::read_to_string(temp.path().join("invocations")).unwrap();
    assert_eq!(
        invocations.lines().collect::<Vec<_>>(),
        vec![
            "exec --json --color never --model gpt-5.4 --config model_reasoning_effort=xhigh --config sandbox_mode=\"danger-full-access\" --config approval_policy=\"never\" --config model_reasoning_summary=concise -",
            "exec resume --json --model gpt-5.4 --config model_reasoning_effort=xhigh --config sandbox_mode=\"danger-full-access\" --config approval_policy=\"never\" --config model_reasoning_summary=concise thread-123 -",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("configured-proxy")).unwrap(),
        "http://user:password@127.0.0.1:7890\n".repeat(4)
    );
    assert!(first_output.events.iter().any(
        |event| matches!(event, OutputEvent::Answer { text } if text.contains("hello from codex"))
    ));
    assert!(
        first_output
            .events
            .iter()
            .all(|event| !format!("{event:?}").contains("thread.started"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_classifies_thinking_progress_and_final_answer() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "printf '%s\\n' ",
            "'{\"type\":\"thread.started\",\"thread_id\":\"thread-123\"}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"id\":\"msg-0\",\"type\":\"agent_message\",\"text\":\"I will inspect the channel path\"}}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"id\":\"reason-1\",\"type\":\"reasoning\",\"text\":\"Inspecting the channel path\"}}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.started\",\"item\":{\"id\":\"cmd-1\",\"type\":\"command_execution\",\"command\":\"cargo test\",\"aggregated_output\":\"\",\"exit_code\":null,\"status\":\"in_progress\"}}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"id\":\"cmd-1\",\"type\":\"command_execution\",\"command\":\"cargo test\",\"aggregated_output\":\"ok\",\"exit_code\":0,\"status\":\"completed\"}}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"id\":\"msg-1\",\"type\":\"agent_message\",\"text\":\"All checks passed\"}}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":1}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, temp.path())).unwrap();
    let mut output = VecAgentOutput::default();

    agent
        .run(
            AgentTask::new("hello"),
            None,
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap();

    assert_eq!(
        output.events,
        vec![
            OutputEvent::Progress {
                id: "msg-0".to_string(),
                text: "I will inspect the channel path".to_string(),
                status: ProgressStatus::Completed,
            },
            OutputEvent::Thinking {
                text: "Inspecting the channel path".to_string(),
            },
            OutputEvent::CommandExecution {
                id: "cmd-1".to_string(),
                command: "cargo test".to_string(),
                status: ProgressStatus::Running,
                exit_code: None,
            },
            OutputEvent::CommandExecution {
                id: "cmd-1".to_string(),
                command: "cargo test".to_string(),
                status: ProgressStatus::Completed,
                exit_code: Some(0),
            },
            OutputEvent::Answer {
                text: "All checks passed".to_string(),
            },
            OutputEvent::Usage(TokenUsage {
                input_tokens: 1,
                cached_input_tokens: 0,
                output_tokens: 1,
                reasoning_output_tokens: 1,
            }),
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_reports_a_missing_session_without_persisting_it() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "printf '%s\\n' ",
            "'Error: thread/resume failed: no rollout found for thread id missing' >&2\n",
            "exit 1\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, temp.path())).unwrap();
    let mut output = VecAgentOutput::default();

    let outcome = completed(
        agent
            .run(
                AgentTask::new("hello"),
                Some("missing".to_string()),
                AgentRunControl::new(),
                &mut output,
            )
            .await
            .unwrap(),
    );

    assert_eq!(outcome.session_update(), &AgentSessionUpdate::NotFound);
    assert!(output.events.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_does_not_publish_backend_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "printf '%s\\n' ",
            "'{\"type\":\"thread.started\",\"thread_id\":\"thread-123\"}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"visible response\"}}'\n",
            "printf '%s\\n' ",
            "'ERROR codex_core::tools::router: internal diagnostic' >&2\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, temp.path())).unwrap();
    let mut output = VecAgentOutput::default();

    agent
        .run(
            AgentTask::new("hello"),
            None,
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap();

    let output = output.answer_text();
    assert!(output.contains("visible response"));
    assert!(!output.contains("codex_core::tools::router"));
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_passes_image_attachments_to_a_resumed_turn() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"$@\" > invocation-args\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "  if [ \"$1\" = \"--image\" ]; then\n",
            "    shift\n",
            "    cp \"$1\" received-image\n",
            "  fi\n",
            "  shift\n",
            "done\n",
            "cat > received-prompt\n",
            "printf '%s\\n' ",
            "'{\"type\":\"thread.started\",\"thread_id\":\"thread-123\"}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"image received\"}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, temp.path())).unwrap();
    let content = TaskContent::new("analyze this image").with_attachment(TaskAttachment::image(
        "trace.png",
        "image/png",
        b"image-bytes".to_vec(),
    ));
    let mut output = VecAgentOutput::default();

    agent
        .run(
            AgentTask::new(content),
            Some("thread-123".to_string()),
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap();

    let args = std::fs::read_to_string(temp.path().join("invocation-args")).unwrap();
    let args = args.lines().collect::<Vec<_>>();
    assert_eq!(&args[..3], ["exec", "resume", "--json"]);
    let image = args.iter().position(|arg| *arg == "--image").unwrap();
    let session = args.iter().position(|arg| *arg == "thread-123").unwrap();
    assert!(image < session);
    assert_eq!(args.last(), Some(&"-"));
    assert_eq!(
        std::fs::read(temp.path().join("received-image")).unwrap(),
        b"image-bytes"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("received-prompt")).unwrap(),
        "analyze this image"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_deletes_its_backend_session() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "script_dir=${0%/*}\n",
            "printf '%s\\n' \"$*\" > \"$script_dir/delete-invocation\"\n",
            "printf '%s' \"$HTTP_PROXY\" > \"$script_dir/delete-proxy\"\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let mut config = agent(AgentType::Codex, &script, temp.path());
    config.proxy = Some(":password@127.0.0.1:7890".parse().unwrap());
    let agent = ConfiguredAgent::from_config(config).unwrap();

    assert_eq!(
        agent
            .delete_session("019f5eb1-cf97-7c71-bf16-b7cff731724a")
            .await
            .unwrap(),
        DeleteSessionOutcome::Deleted
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("delete-invocation")).unwrap(),
        "delete --force 019f5eb1-cf97-7c71-bf16-b7cff731724a\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("delete-proxy")).unwrap(),
        "http://:password@127.0.0.1:7890"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_maps_all_supported_json_events_and_stream_boundaries() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "printf 'plain output\\r\\n'\n",
            "printf '%s\\n' '",
            r#"{"type":"thread.started","thread_id":"thread-events"}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"intermediate answer"}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.completed","item":{"id":"reason-1","type":"reasoning","text":"  compact   reasoning  "}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.started","item":{"type":"command_execution","command":"echo `unsafe` && /bin/bash -lc \"pwd && rg --files -g !target -g !**/.git/** | sed -n 1,260p && git status --short && git log -1 --oneline && find spec -maxdepth 2 -type f -print && echo end-of-command\"","status":"declined"}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.updated","item":{"id":"files-1","type":"file_change","changes":[{},{}],"status":"in_progress"}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.completed","item":{"id":"todo-1","type":"todo_list","items":[{"completed":true},{"completed":false}]}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.started","item":{"id":"mcp-1","type":"mcp_tool_call","server":"docs","tool":"search"}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.completed","item":{"id":"search-1","type":"web_search","query":"find `docs`"}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"item.completed","item":{"type":"error"}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"turn.failed","error":{"message":"turn failed detail"}}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"error"}"#,
            "'\n",
            "printf '%s\\n' '",
            r#"{"type":"turn.completed","usage":{"input_tokens":9,"output_tokens":4}}"#,
            "'\n",
            "printf '%s' '",
            r#"{"type":"item.completed","item":{"id":"final-1","type":"agent_message","text":"final without newline"}}"#,
            "'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, temp.path())).unwrap();
    let mut output = VecAgentOutput::default();

    let outcome = completed(
        agent
            .run(
                AgentTask::new("exercise event mapping"),
                None,
                AgentRunControl::new(),
                &mut output,
            )
            .await
            .unwrap(),
    );

    assert_eq!(
        outcome.session_update(),
        &AgentSessionUpdate::Set("thread-events".to_string())
    );
    assert!(output.events.contains(&OutputEvent::Answer {
        text: "plain output\n".to_string(),
    }));
    assert!(output.events.contains(&OutputEvent::Progress {
        id: "agent-message".to_string(),
        text: "intermediate answer".to_string(),
        status: ProgressStatus::Completed,
    }));
    assert!(output.events.contains(&OutputEvent::Thinking {
        text: "compact reasoning".to_string(),
    }));
    assert!(output.events.contains(&OutputEvent::CommandExecution {
        id: "codex-progress".to_string(),
        command: r#"echo `unsafe` && /bin/bash -lc "pwd && rg --files -g !target -g !**/.git/** | sed -n 1,260p && git status --short && git log -1 --oneline && find spec -maxdepth 2 -type f -print && echo end-of-command""#.to_string(),
        status: ProgressStatus::Failed,
        exit_code: None,
    }));
    assert!(output.events.contains(&OutputEvent::Progress {
        id: "files-1".to_string(),
        text: "Changed 2 file(s)".to_string(),
        status: ProgressStatus::Running,
    }));
    assert!(output.events.contains(&OutputEvent::Progress {
        id: "todo-1".to_string(),
        text: "Plan progress: 1/2".to_string(),
        status: ProgressStatus::Completed,
    }));
    assert!(output.events.contains(&OutputEvent::Progress {
        id: "mcp-1".to_string(),
        text: "Call `docs/search`".to_string(),
        status: ProgressStatus::Running,
    }));
    assert!(output.events.contains(&OutputEvent::Progress {
        id: "search-1".to_string(),
        text: "Search `find 'docs'`".to_string(),
        status: ProgressStatus::Completed,
    }));
    assert!(output.events.contains(&OutputEvent::Progress {
        id: "codex-progress".to_string(),
        text: "codex item failed".to_string(),
        status: ProgressStatus::Failed,
    }));
    assert!(output.events.contains(&OutputEvent::Answer {
        text: "turn failed detail".to_string(),
    }));
    assert!(output.events.contains(&OutputEvent::Answer {
        text: "codex execution failed".to_string(),
    }));
    assert!(output.events.contains(&OutputEvent::Usage(TokenUsage {
        input_tokens: 9,
        cached_input_tokens: 0,
        output_tokens: 4,
        reasoning_output_tokens: 0,
    })));
    assert!(output.events.contains(&OutputEvent::Answer {
        text: "final without newline".to_string(),
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_truncates_intermediate_messages_and_ignores_unknown_events() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "long=$(printf '%0250d' 0 | tr '0' 'x')\n",
            "printf '%s\\n' \"{\\\"type\\\":\\\"item.completed\\\",\\\"item\\\":{\\\"id\\\":\\\"long-message\\\",\\\"type\\\":\\\"agent_message\\\",\\\"text\\\":\\\"  $long  \\\"}}\"\n",
            "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"future_item\"}}'\n",
            "printf '%s\\n' '{\"type\":\"item.started\"}'\n",
            "printf '%s\\n' '{\"type\":\"future.event\"}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, temp.path())).unwrap();
    let mut output = VecAgentOutput::default();
    let outcome = completed(
        agent
            .run(
                AgentTask::new("exercise forward-compatible events"),
                None,
                AgentRunControl::new(),
                &mut output,
            )
            .await
            .unwrap(),
    );

    assert_eq!(outcome.session_update(), &AgentSessionUpdate::Unchanged);
    let truncated = output
        .events
        .iter()
        .find_map(|event| match event {
            OutputEvent::Progress { id, text, .. } if id == "long-message" => Some(text),
            _ => None,
        })
        .unwrap();
    assert_eq!(truncated.chars().count(), 243);
    assert!(truncated.ends_with("..."));
}

#[cfg(unix)]
#[tokio::test]
async fn codex_agent_reports_delete_failures_with_stdout_and_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'stdout detail\\n'\nprintf 'stderr detail\\n' >&2\nexit 7\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, temp.path())).unwrap();
    let error = agent.delete_session("broken-session").await.unwrap_err();
    let message = error.to_string();

    assert!(message.contains("exit_code=7"));
    assert!(message.contains("stdout detail"));
    assert!(message.contains("stderr detail"));
}
