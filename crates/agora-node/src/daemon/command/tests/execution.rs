use super::*;

#[test]
fn command_execution_conversions_and_handler_identity_are_explicit() {
    let dispatch = AgentDispatch::new(Vec::new(), TaskContent::new("prompt"));
    let execution: CommandExecution = dispatch.into();
    let CommandExecution::Dispatch(dispatch) = execution else {
        panic!("expected dispatch execution");
    };
    let (agents, content) = dispatch.into_parts();
    assert!(agents.is_empty());
    assert_eq!(content.text(), "prompt");

    let execution: CommandExecution = None::<ChannelReply>.into();
    assert!(matches!(execution, CommandExecution::Reply(None)));

    let handler = CommandHandler::new(|_, _| async { Ok(None::<ChannelReply>) });
    let clone = handler.clone();
    let other = CommandHandler::new(|_, _| async { Ok(None::<ChannelReply>) });
    assert_eq!(format!("{handler:?}"), "CommandHandler");
    assert!(handler == clone);
    assert!(handler != other);
}

#[test]
fn neutral_command_request_and_button_preserve_data() {
    let request = CommandRequest::new(["ask", "disable"]).with_argument("agent_name", "codex-dev");
    let input = ChannelTaskInput::Command(request.clone());
    let button = ChannelButton::new("Disable", ChannelButtonStyle::Default, request.clone());

    assert_eq!(request.path(), ["ask", "disable"]);
    assert_eq!(request.argument("agent_name"), Some("codex-dev"));
    assert_eq!(input.command(), Some(&request));
    assert_eq!(button.text(), "Disable");
    assert_eq!(button.style(), ChannelButtonStyle::Default);
    assert_eq!(button.command(), &request);
}

#[tokio::test]
async fn silent_command_completes_without_a_channel_reply() {
    let temp = tempfile::tempdir().unwrap();
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("store.db")).unwrap());
    let mut registry = CommandRegistry::new();
    registry
        .register(
            CommandNode::new("silent", "Do not reply").handler(CommandHandler::new(|_, _| async {
                Ok(None::<ChannelReply>)
            })),
        )
        .unwrap();
    let commands = CommandRuntime { registry };
    let replies = Arc::new(Mutex::new(Vec::new()));
    let channel = CommandTestChannel::new(Arc::clone(&replies));
    let mut runs = tokio::task::JoinSet::new();

    Daemon::route_channel_task(
        &channel,
        &[],
        &dispatcher,
        &commands,
        CommandTestTask::new("silent", "chat-1", "/silent"),
        &mut runs,
    )
    .await
    .unwrap();

    assert!(replies.lock().unwrap().is_empty());
    assert!(runs.is_empty());
}

#[tokio::test]
async fn stop_reports_empty_session_and_named_agent_states() {
    let runtime = isolated_command_runtime();

    for (input, expected) in [
        ("/stop", crate::i18n::no_running_agents().to_string()),
        ("/stop codex", crate::i18n::no_running_agent("codex")),
    ] {
        let outcome = runtime
            .handle(
                "lark",
                "chat-1",
                &[],
                &ChannelTaskInput::Message(TaskContent::new(input)),
            )
            .await
            .unwrap();
        let CommandOutcome::Reply(Some(reply)) = outcome else {
            panic!("expected command reply");
        };
        assert_eq!(reply, ChannelReply::new(expected));
    }
}

#[tokio::test]
async fn interrupt_callback_stops_only_its_registered_agent_run() {
    let scheduler = ExecutionScheduler::default();
    let key = SessionKey::new("codex", IsolationScope::session("lark", "chat-1"));
    let (_target_ticket, target) = scheduled_run(
        &scheduler,
        ExecutionScope::new("lark", "chat-1", key.clone()),
    );
    let (_duplicate_ticket, duplicate) =
        scheduled_run(&scheduler, ExecutionScope::new("lark", "chat-1", key));
    let interrupt_control = target.clone();
    let interrupt = InterruptCallback::new(move || interrupt_control.stop());

    assert!(interrupt.trigger());
    assert_eq!(target.cancelled().await, AgentRunCancellation::Stopped);
    assert!(
        timeout(Duration::from_millis(20), duplicate.cancelled())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn execution_ticket_combines_fifo_admission_and_run_cancellation() {
    let scheduler = ExecutionScheduler::default();
    let scope = ExecutionScope::new(
        "lark",
        "chat-1",
        SessionKey::new("codex", IsolationScope::session("lark", "chat-1")),
    );
    let first = scheduler.enqueue(scope.clone());
    let mut second = scheduler.enqueue(scope);
    let first_control = first.control();
    let second_control = second.control();

    assert_eq!(first.ahead(), 0);
    assert_eq!(second.ahead(), 1);
    assert!(first_control.stop());
    assert_eq!(
        first_control.cancelled().await,
        AgentRunCancellation::Stopped
    );
    assert!(
        timeout(Duration::from_millis(20), second_control.cancelled())
            .await
            .is_err()
    );

    drop(first);
    assert_eq!(second.changed().await.unwrap(), 0);
}

#[tokio::test]
async fn scheduler_barrier_serializes_reset_without_becoming_an_agent_run() {
    let scheduler = ExecutionScheduler::default();
    let key = SessionKey::new("codex", IsolationScope::session("lark", "chat-1"));
    let run = scheduler.enqueue(ExecutionScope::new("lark", "chat-1", key.clone()));
    let mut barrier = scheduler.barrier(&key);

    assert_eq!(barrier.ahead(), 1);
    assert_eq!(scheduler.interrupt_all(), 1);
    drop(run);
    barrier.wait_until_front().await.unwrap();
    timeout(Duration::from_millis(20), scheduler.wait_until_empty())
        .await
        .unwrap();
}

#[tokio::test]
async fn execution_stops_all_agents_only_in_the_current_session() {
    let scheduler = ExecutionScheduler::default();
    let (_codex_ticket, codex) = scheduled_run(&scheduler, run_scope("lark", "chat-1", "codex"));
    let (_reviewer_ticket, reviewer) =
        scheduled_run(&scheduler, run_scope("lark", "chat-1", "reviewer"));
    let (_other_ticket, other_session) =
        scheduled_run(&scheduler, run_scope("lark", "chat-2", "codex"));

    assert_eq!(
        scheduler.stop("lark", "chat-1", None),
        vec!["codex".to_string(), "reviewer".to_string()]
    );
    timeout(Duration::from_millis(50), codex.cancelled())
        .await
        .unwrap();
    timeout(Duration::from_millis(50), reviewer.cancelled())
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(20), other_session.cancelled())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn execution_stops_only_the_named_agent() {
    let scheduler = ExecutionScheduler::default();
    let (_codex_ticket, codex) = scheduled_run(&scheduler, run_scope("lark", "chat-1", "codex"));
    let (_reviewer_ticket, reviewer) =
        scheduled_run(&scheduler, run_scope("lark", "chat-1", "reviewer"));

    assert_eq!(
        scheduler.stop("lark", "chat-1", Some("codex")),
        vec!["codex".to_string()]
    );
    timeout(Duration::from_millis(50), codex.cancelled())
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(20), reviewer.cancelled())
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_a_card_run_advances_the_next_queued_task() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("slow-agent");
    std::fs::write(&script, "#!/bin/sh\ncat >/dev/null\nexec sleep 30\n").unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let agent = ConfiguredAgent::from_config(AgentConfig {
        name: "codex-dev".to_string(),
        isolate: IsolateMode::Session,
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
    let events = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let interrupts = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(Vec::new()));
    let channel = CommandTestChannel {
        tasks: VecDeque::new(),
        received: None,
        events: Arc::clone(&events),
        contexts: Arc::clone(&contexts),
        interrupts: Arc::clone(&interrupts),
        replies: Arc::clone(&replies),
    };
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("store.db")).unwrap());
    let mut runs = tokio::task::JoinSet::new();

    Daemon::route_channel_task(
        &channel,
        std::slice::from_ref(&agent),
        &dispatcher,
        &command_runtime(&dispatcher),
        CommandTestTask::new("task-1", "chat-1", "first task"),
        &mut runs,
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(2), async {
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

    Daemon::route_channel_task(
        &channel,
        std::slice::from_ref(&agent),
        &dispatcher,
        &command_runtime(&dispatcher),
        CommandTestTask::new("task-2", "chat-1", "second task"),
        &mut runs,
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(2), async {
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

    assert!(interrupts.lock().unwrap()[0].trigger());
    timeout(Duration::from_secs(2), async {
        loop {
            let started = events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, RunEvent::Started { .. }))
                .count();
            if started == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    assert!(interrupts.lock().unwrap()[1].trigger());
    timeout(Duration::from_secs(2), async {
        while let Some(result) = runs.join_next().await {
            result.unwrap().unwrap();
        }
    })
    .await
    .unwrap();

    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RunEvent::Started { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event == &&RunEvent::Stopped)
            .count(),
        2
    );
    assert!(events.contains(&RunEvent::Queued { ahead: 1 }));
    assert!(replies.lock().unwrap().is_empty());
    assert_eq!(
        contexts.lock().unwrap().as_slice(),
        ["codex-dev", "codex-dev"]
    );
}

#[tokio::test]
async fn execution_stops_every_run_using_a_reset_session_key() {
    let scheduler = ExecutionScheduler::default();
    let shared = SessionKey::new("codex", IsolationScope::Shared);
    let other = SessionKey::new("reviewer", IsolationScope::Shared);
    let (_lark_ticket, lark) = scheduled_run(
        &scheduler,
        ExecutionScope::new("lark", "chat-1", shared.clone()),
    );
    let (_telegram_ticket, telegram) = scheduled_run(
        &scheduler,
        ExecutionScope::new("telegram", "chat-2", shared.clone()),
    );
    let (_reviewer_ticket, reviewer) =
        scheduled_run(&scheduler, ExecutionScope::new("lark", "chat-1", other));

    assert_eq!(
        scheduler.stop_session_keys(std::slice::from_ref(&shared)),
        vec!["codex".to_string()]
    );
    assert_eq!(lark.cancelled().await, AgentRunCancellation::Stopped);
    assert_eq!(telegram.cancelled().await, AgentRunCancellation::Stopped);
    assert!(
        timeout(Duration::from_millis(20), reviewer.cancelled())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn execution_interrupts_every_run_during_process_shutdown() {
    let scheduler = ExecutionScheduler::default();
    let (_codex_ticket, codex) = scheduled_run(&scheduler, run_scope("lark", "chat-1", "codex"));
    let (_reviewer_ticket, reviewer) =
        scheduled_run(&scheduler, run_scope("lark", "chat-2", "reviewer"));

    assert_eq!(scheduler.interrupt_all(), 2);
    assert_eq!(codex.cancelled().await, AgentRunCancellation::Interrupted);
    assert_eq!(
        reviewer.cancelled().await,
        AgentRunCancellation::Interrupted
    );
}
