use super::*;

#[cfg(unix)]
#[tokio::test]
async fn persists_and_serializes_session_by_channel_and_agent() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"$*\" >> invocations\n",
            "cat >/dev/null\n",
            "sleep 0.1\n",
            "printf '%s\\n' ",
            "'{\"type\":\"thread.started\",\"thread_id\":\"thread-123\"}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let channel = RecordingChannel {
        contexts: Arc::new(Mutex::new(Vec::new())),
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
    let agent = AgentConfig {
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
    };
    let agent = ConfiguredAgent::from_config(agent).unwrap();
    let store_key = agent.store_session_key(&channel.identity(), TestTask.session_id());

    let first = dispatcher.dispatch_channel_task(&channel, vec![agent.clone()], TestTask);
    let second = dispatcher.dispatch_channel_task(&channel, vec![agent], TestTask);
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();

    let invocations = std::fs::read_to_string(temp.path().join("invocations")).unwrap();
    assert_eq!(
        invocations.lines().collect::<Vec<_>>(),
        vec![
            "exec --json --color never --skip-git-repo-check --config model_reasoning_summary=concise -",
            "exec resume --json --skip-git-repo-check --config model_reasoning_summary=concise thread-123 -",
        ]
    );
    assert_eq!(
        store.get(&store_key).unwrap().as_deref(),
        Some("thread-123")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn none_isolation_queues_and_resumes_across_channels() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::time::{Duration, timeout};

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"$*\" >> invocations\n",
            "cat >/dev/null\n",
            "while [ ! -f release ]; do sleep 0.01; done\n",
            "printf '%s\\n' ",
            "'{\"type\":\"thread.started\",\"thread_id\":\"thread-shared\"}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
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
    let store_key =
        agent.store_session_key(&ChannelIdentity::new("lark1", "test", "lark1"), "chat-1");
    let first_events = Arc::new(Mutex::new(Vec::new()));
    let second_events = Arc::new(Mutex::new(Vec::new()));

    let first = tokio::spawn({
        let dispatcher = dispatcher.clone();
        let agent = agent.clone();
        let events = Arc::clone(&first_events);
        async move {
            dispatcher
                .dispatch_channel_task(
                    &ScopedChannel::with_events("lark1", events),
                    vec![agent],
                    ScopedTask::new("task-1", "chat-1"),
                )
                .await
        }
    });
    timeout(Duration::from_secs(5), async {
        loop {
            if first_events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, RunEvent::Started { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let second = tokio::spawn({
        let dispatcher = dispatcher.clone();
        let events = Arc::clone(&second_events);
        async move {
            dispatcher
                .dispatch_channel_task(
                    &ScopedChannel::with_events("telegram1", events),
                    vec![agent],
                    ScopedTask::new("task-2", "chat-2"),
                )
                .await
        }
    });

    timeout(Duration::from_secs(5), async {
        loop {
            if second_events
                .lock()
                .unwrap()
                .contains(&RunEvent::Queued { ahead: 1 })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    std::fs::write(temp.path().join("release"), "").unwrap();

    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    let invocations = std::fs::read_to_string(temp.path().join("invocations")).unwrap();
    assert_eq!(
        invocations.lines().collect::<Vec<_>>(),
        vec![
            "exec --json --color never --skip-git-repo-check --config model_reasoning_summary=concise -",
            "exec resume --json --skip-git-repo-check --config model_reasoning_summary=concise thread-shared -",
        ]
    );
    assert!(
        second_events
            .lock()
            .unwrap()
            .contains(&RunEvent::Queued { ahead: 1 })
    );
    assert_eq!(
        store.get(&store_key).unwrap().as_deref(),
        Some("thread-shared")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_isolation_separates_backend_sessions_and_reuses_workspace() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "script_dir=${0%/*}\n",
            "invocations=\"$script_dir/invocations\"\n",
            "printf '%s\\n' \"$*\" >> \"$invocations\"\n",
            "pwd >> \"$script_dir/workdirs\"\n",
            "count=$(wc -l < \"$invocations\" | tr -d ' ')\n",
            "cat >/dev/null\n",
            "printf '{\"type\":\"thread.started\",\"thread_id\":\"thread-%s\"}\\n' \"$count\"\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let invocations = temp.path().join("invocations");
    let workdirs = temp.path().join("workdirs");
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
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
    let channel = ScopedChannel::new("lark1");
    let first_store_key = agent.store_session_key(&channel.identity(), "chat-1");
    let second_store_key = agent.store_session_key(&channel.identity(), "chat-2");

    dispatcher
        .dispatch_channel_task(
            &channel,
            vec![agent.clone()],
            ScopedTask::new("task-1", "chat-1"),
        )
        .await
        .unwrap();
    dispatcher
        .dispatch_channel_task(&channel, vec![agent], ScopedTask::new("task-2", "chat-2"))
        .await
        .unwrap();

    let invocations = std::fs::read_to_string(invocations).unwrap();
    assert_eq!(
        invocations.lines().collect::<Vec<_>>(),
        vec![
            "exec --json --color never --skip-git-repo-check --config model_reasoning_summary=concise -",
            "exec --json --color never --skip-git-repo-check --config model_reasoning_summary=concise -",
        ]
    );
    assert_eq!(
        store.get(&first_store_key).unwrap().as_deref(),
        Some("thread-1")
    );
    assert_eq!(
        store.get(&second_store_key).unwrap().as_deref(),
        Some("thread-2")
    );
    let workdirs = std::fs::read_to_string(workdirs).unwrap();
    let expected_workdir = std::fs::canonicalize(temp.path()).unwrap();
    let expected_workdir = expected_workdir.to_string_lossy();
    assert_eq!(
        workdirs.lines().collect::<Vec<_>>(),
        vec![expected_workdir.as_ref(), expected_workdir.as_ref()]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn replaces_a_missing_agent_session_with_a_new_session() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"$*\" >> invocations\n",
            "cat >/dev/null\n",
            "case \"$*\" in\n",
            "  *\" missing \"*)\n",
            "    printf '%s\\n' ",
            "'Error: thread/resume failed: no rollout found for thread id missing' >&2\n",
            "    exit 1\n",
            "    ;;\n",
            "esac\n",
            "printf '%s\\n' ",
            "'{\"type\":\"thread.started\",\"thread_id\":\"thread-new\"}'\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
    let channel = RecordingChannel {
        contexts: Arc::new(Mutex::new(Vec::new())),
        events: Arc::new(Mutex::new(Vec::new())),
    };
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
    let store_key = agent.store_session_key(&channel.identity(), TestTask.session_id());
    store.observe(&store_key, None, "missing").unwrap();

    dispatcher
        .dispatch_channel_task(&channel, vec![agent], TestTask)
        .await
        .unwrap();

    let invocations = std::fs::read_to_string(temp.path().join("invocations")).unwrap();
    assert_eq!(
        invocations.lines().collect::<Vec<_>>(),
        vec![
            "exec resume --json --skip-git-repo-check --config model_reasoning_summary=concise missing -",
            "exec --json --color never --skip-git-repo-check --config model_reasoning_summary=concise -",
        ]
    );
    assert_eq!(
        store.get(&store_key).unwrap().as_deref(),
        Some("thread-new")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_a_successful_fresh_codex_run_without_a_session_id() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "printf '%s\\n' ",
            "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n",
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let dispatcher = AgentDispatcher::new(store.clone());
    let channel = RecordingChannel {
        contexts: Arc::new(Mutex::new(Vec::new())),
        events: Arc::new(Mutex::new(Vec::new())),
    };
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
    let store_key = agent.store_session_key(&channel.identity(), TestTask.session_id());

    let error = dispatcher
        .dispatch_channel_task(&channel, vec![agent], TestTask)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("without a session id"));
    assert_eq!(store.get(&store_key).unwrap(), None);
}
