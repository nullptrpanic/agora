use super::*;

#[tokio::test]
async fn channel_loop_routes_stop_without_sending_it_to_the_agent() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("slow-agent");
    std::fs::write(&script, "#!/bin/sh\ncat >/dev/null\nexec sleep 30\n").unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(Vec::new()));
    let channel = CommandTestChannel {
        tasks: VecDeque::from([
            CommandTestTask::new("task-1", "chat-1", "long running task"),
            CommandTestTask::new("task-2", "chat-1", "/stop"),
        ]),
        received: None,
        events: Arc::clone(&events),
        contexts: Arc::clone(&contexts),
        interrupts: Arc::new(Mutex::new(Vec::new())),
        replies: Arc::clone(&replies),
    };
    let agent = ConfiguredAgent::from_config(AgentConfig {
        name: "codex-dev".to_string(),
        isolate: IsolateMode::None,
        workspace: temp.path().to_string_lossy().into_owned(),
        agent_type: AgentType::Custom,
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
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("store.db")).unwrap());
    let commands = Arc::new(command_runtime(&dispatcher));

    let daemon = tokio::spawn(Daemon::run_channel(
        channel,
        vec![agent],
        dispatcher,
        commands,
    ));
    timeout(Duration::from_secs(2), async {
        loop {
            if events.lock().unwrap().contains(&RunEvent::Stopped) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    assert_eq!(contexts.lock().unwrap().as_slice(), ["codex-dev"]);
    assert_eq!(
        replies.lock().unwrap().as_slice(),
        [ChannelReply::new("已停止 1 个 Agent：codex-dev。")]
    );
    daemon.abort();
}

#[tokio::test]
async fn channel_loop_keeps_receiving_while_reset_waits_for_its_barrier() {
    let temp = tempfile::tempdir().unwrap();
    let replies = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::new(AtomicUsize::new(0));
    let channel = CommandTestChannel {
        tasks: VecDeque::from([
            CommandTestTask::new("reset", "chat-1", "/reset"),
            CommandTestTask::new("help", "chat-1", "/help"),
        ]),
        received: Some(Arc::clone(&received)),
        events: Arc::new(Mutex::new(Vec::new())),
        contexts: Arc::new(Mutex::new(Vec::new())),
        interrupts: Arc::new(Mutex::new(Vec::new())),
        replies: Arc::clone(&replies),
    };
    let agent = command_test_agent("codex-dev", temp.path());
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("nonblocking-reset.db")).unwrap());
    let blocker = dispatcher
        .scheduler
        .enqueue(run_scope("lark", "chat-1", "codex-dev"));
    let commands = Arc::new(command_runtime(&dispatcher));

    let daemon = tokio::spawn(Daemon::run_channel(
        channel,
        vec![agent],
        dispatcher,
        commands,
    ));

    timeout(Duration::from_secs(1), async {
        while received.load(Ordering::Acquire) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(replies.lock().unwrap().is_empty());

    drop(blocker);
    daemon.abort();
}
