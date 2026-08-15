use super::*;

#[cfg(unix)]
#[tokio::test]
async fn updates_queue_depth_before_starting_serialized_agent_runs() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::time::{Duration, timeout};

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "printf 'started\\n' >> invocations\n",
            "cat >/dev/null\n",
            "while [ ! -f release ]; do sleep 0.01; done\n",
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

    let events = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("queued-store.db")).unwrap());
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

    let first = tokio::spawn({
        let dispatcher = dispatcher.clone();
        let agent = agent.clone();
        let events = Arc::clone(&events);
        let contexts = Arc::clone(&contexts);
        async move {
            dispatcher
                .dispatch_channel_task(
                    &RecordingChannel { contexts, events },
                    vec![agent],
                    TestTask,
                )
                .await
        }
    });
    timeout(Duration::from_secs(5), async {
        loop {
            if events
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
        let agent = agent.clone();
        let events = Arc::clone(&events);
        let contexts = Arc::clone(&contexts);
        async move {
            dispatcher
                .dispatch_channel_task(
                    &RecordingChannel { contexts, events },
                    vec![agent],
                    TestTask,
                )
                .await
        }
    });
    timeout(Duration::from_secs(5), async {
        loop {
            if events
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

    let third = tokio::spawn({
        let dispatcher = dispatcher.clone();
        let events = Arc::clone(&events);
        let contexts = Arc::clone(&contexts);
        let agent = agent.clone();
        async move {
            dispatcher
                .dispatch_channel_task(
                    &RecordingChannel { contexts, events },
                    vec![agent],
                    TestTask,
                )
                .await
        }
    });
    timeout(Duration::from_secs(5), async {
        loop {
            if events
                .lock()
                .unwrap()
                .contains(&RunEvent::Queued { ahead: 2 })
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
    third.await.unwrap().unwrap();

    let events = events.lock().unwrap();
    let queued_two = events
        .iter()
        .position(|event| event == &RunEvent::Queued { ahead: 2 })
        .unwrap();
    let queued_one = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| (event == &RunEvent::Queued { ahead: 1 }).then_some(index))
        .collect::<Vec<_>>();
    let started = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| matches!(event, RunEvent::Started { .. }).then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 3);
    assert_eq!(queued_one.len(), 2);
    assert!(started[0] < queued_one[0]);
    assert!(queued_one[0] < queued_two);
    assert!(queued_two < queued_one[1]);
    assert!(queued_one[1] < started[2]);
}
