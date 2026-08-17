use super::*;

#[cfg(unix)]
#[tokio::test]
async fn reset_stops_the_scope_deletes_the_session_and_starts_fresh_next_time() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    let invocations = temp.path().join("invocations");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "script_dir=${0%/*}\n",
            "printf '%s\\n' \"$*\" >> \"$script_dir/invocations\"\n",
            "if [ \"$1\" = delete ]; then exit 0; fi\n",
            "cat >/dev/null\n",
            "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"new-session\"}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent = ConfiguredAgent::from_config(AgentConfig {
        name: "codex-dev".to_string(),
        isolate: IsolateMode::None,
        workspace: temp.path().to_string_lossy().into_owned(),
        agent_type: AgentType::Codex,
        path: script.to_string_lossy().into_owned(),
        model: None,
        effort: None,
        agent_sandbox: None,
        proxy: None,
        timeout_seconds: 3600,
        max_output_bytes: 64 * 1024 * 1024,
        subscribe: Vec::new(),
    })
    .unwrap();
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
    let key = SessionKey::new(agent.name(), agent.isolation_scope("lark", "chat-1"));
    store.save(&key, "old-session").unwrap();

    let active_ticket = dispatcher.scheduler.enqueue(ExecutionScope::new(
        "telegram",
        "chat-2",
        key.clone(),
        std::path::PathBuf::from("/tmp/agora-command-test"),
    ));
    let active_control = active_ticket.control();
    let active_run = tokio::spawn(async move {
        assert_eq!(
            active_control.cancelled().await,
            AgentRunCancellation::Stopped
        );
        drop(active_ticket);
    });

    let replies = Arc::new(Mutex::new(Vec::new()));
    let channel = CommandTestChannel {
        tasks: VecDeque::new(),
        received: None,
        events: Arc::new(Mutex::new(Vec::new())),
        contexts: Arc::new(Mutex::new(Vec::new())),
        interrupts: Arc::new(Mutex::new(Vec::new())),
        replies: Arc::clone(&replies),
    };
    let mut runs = tokio::task::JoinSet::new();
    Daemon::route_channel_task(
        &channel,
        std::slice::from_ref(&agent),
        &dispatcher,
        &command_runtime(&dispatcher),
        CommandTestTask::new("reset", "chat-1", "/reset"),
        &mut runs,
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(1), active_run)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(store.get(&key).unwrap(), None);
    assert_eq!(
        replies.lock().unwrap().as_slice(),
        [ChannelReply::new("重置成功。")]
    );

    let mut output = IgnoreAgentOutput;
    dispatcher
        .execute_agent(
            &key,
            &agent,
            AgentTask::new("next task"),
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap();

    assert_eq!(store.get(&key).unwrap().as_deref(), Some("new-session"));
    let invocations = std::fs::read_to_string(invocations).unwrap();
    assert!(invocations.contains("delete --force old-session\n"));
    assert!(invocations.lines().any(|line| line.starts_with("exec ")));
    assert!(!invocations.lines().any(|line| line.contains(" resume ")));
}

#[cfg(unix)]
#[tokio::test]
async fn reset_preserves_the_mapping_when_backend_deletion_fails() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'backend refused deletion' >&2\nexit 7\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent = ConfiguredAgent::from_config(AgentConfig {
        name: "codex-dev".to_string(),
        isolate: IsolateMode::Session,
        workspace: temp.path().to_string_lossy().into_owned(),
        agent_type: AgentType::Codex,
        path: script.to_string_lossy().into_owned(),
        model: None,
        effort: None,
        agent_sandbox: None,
        proxy: None,
        timeout_seconds: 3600,
        max_output_bytes: 64 * 1024 * 1024,
        subscribe: Vec::new(),
    })
    .unwrap();
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
    let key = SessionKey::new(agent.name(), agent.isolation_scope("lark", "chat-1"));
    store.save(&key, "old-session").unwrap();

    let replies = Arc::new(Mutex::new(Vec::new()));
    let channel = CommandTestChannel {
        tasks: VecDeque::new(),
        received: None,
        events: Arc::new(Mutex::new(Vec::new())),
        contexts: Arc::new(Mutex::new(Vec::new())),
        interrupts: Arc::new(Mutex::new(Vec::new())),
        replies: Arc::clone(&replies),
    };
    let mut runs = tokio::task::JoinSet::new();
    Daemon::route_channel_task(
        &channel,
        std::slice::from_ref(&agent),
        &dispatcher,
        &command_runtime(&dispatcher),
        CommandTestTask::new("reset", "chat-1", "/reset"),
        &mut runs,
    )
    .await
    .unwrap();

    assert_eq!(store.get(&key).unwrap().as_deref(), Some("old-session"));
    assert_eq!(
        replies.lock().unwrap().as_slice(),
        [ChannelReply::new("以下 Agent 重置失败：codex-dev。")]
    );
}

#[tokio::test]
async fn reset_removes_the_mapping_when_backend_deletion_is_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    let agent = ConfiguredAgent::from_config(AgentConfig {
        name: "custom-dev".to_string(),
        isolate: IsolateMode::Session,
        workspace: temp.path().to_string_lossy().into_owned(),
        agent_type: AgentType::Custom,
        path: "/bin/cat".to_string(),
        model: None,
        effort: None,
        agent_sandbox: None,
        proxy: None,
        timeout_seconds: 3600,
        max_output_bytes: 64 * 1024 * 1024,
        subscribe: Vec::new(),
    })
    .unwrap();
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
    let key = SessionKey::new(agent.name(), agent.isolation_scope("lark", "chat-1"));
    store.save(&key, "custom-session").unwrap();
    let replies = Arc::new(Mutex::new(Vec::new()));
    let channel = CommandTestChannel::new(Arc::clone(&replies));
    let mut runs = tokio::task::JoinSet::new();

    Daemon::route_channel_task(
        &channel,
        std::slice::from_ref(&agent),
        &dispatcher,
        &command_runtime(&dispatcher),
        CommandTestTask::new("reset", "chat-1", "/reset"),
        &mut runs,
    )
    .await
    .unwrap();

    assert_eq!(store.get(&key).unwrap(), None);
    assert_eq!(
        replies.lock().unwrap().as_slice(),
        [ChannelReply::new("重置成功。")]
    );
}

#[tokio::test]
async fn reset_succeeds_when_no_session_mapping_exists() {
    let temp = tempfile::tempdir().unwrap();
    let agent = command_test_agent("custom-dev", temp.path());
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("store.db")).unwrap());
    let runtime = command_runtime(&dispatcher);

    let outcome = runtime
        .handle(
            "lark",
            "chat-1",
            &[agent],
            &ChannelTaskInput::Message(TaskContent::new("/reset")),
        )
        .await
        .unwrap();

    let CommandOutcome::Reply(Some(reply)) = outcome else {
        panic!("expected reset reply");
    };
    assert_eq!(reply, ChannelReply::new("重置成功。"));
}
