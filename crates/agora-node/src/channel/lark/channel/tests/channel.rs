use super::*;
use crate::channel::test_http::{HttpMockServer, MockResponse, enable_test_logging};
use crate::config::{
    ChannelGroupPermissionConfig, ChannelPermissionConfig, ChannelUserPermissionConfig,
};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

fn api() -> LarkApi {
    LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-channel-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        "http://127.0.0.1:1".to_string(),
    )
    .unwrap()
}

fn message(message_type: &str) -> LarkMessageEvent {
    LarkMessageEvent {
        id: "evt-message".to_string(),
        message_id: "om-message".to_string(),
        chat_id: "oc-chat".to_string(),
        chat_type: "group".to_string(),
        sender_id: "ou-user".to_string(),
        message_type: message_type.to_string(),
        content: "hello".to_string(),
        image_keys: Vec::new(),
        mention_ids: Vec::new(),
        mention_keys: HashMap::new(),
    }
}

fn permission(users: &[&str], groups: &[(&str, bool)]) -> ChannelPermissionConfig {
    ChannelPermissionConfig {
        users: users
            .iter()
            .map(|id| ChannelUserPermissionConfig {
                id: (*id).to_string(),
            })
            .collect(),
        groups: groups
            .iter()
            .map(|(id, require_mention)| ChannelGroupPermissionConfig {
                id: (*id).to_string(),
                require_mention: *require_mention,
            })
            .collect(),
    }
}

fn event_receiver(events: impl IntoIterator<Item = LarkEvent>) -> LarkWebSocketReceiver {
    let events = events.into_iter().collect::<Vec<_>>();
    let (sender, receiver) = mpsc::channel(events.len().max(1));
    for event in events {
        let (delivery, _) = LarkDelivery::new(event);
        sender.try_send(delivery).unwrap();
    }
    drop(sender);
    LarkWebSocketReceiver {
        events: receiver,
        task: None,
    }
}

fn acknowledged_event_receiver(
    event: LarkEvent,
) -> (
    LarkWebSocketReceiver,
    tokio::sync::oneshot::Receiver<u16>,
    tokio::time::Instant,
) {
    let (sender, events) = mpsc::channel(1);
    let (delivery, acknowledged) = LarkDelivery::new(event);
    let deadline = delivery.deadline();
    sender.try_send(delivery).unwrap();
    drop(sender);
    (
        LarkWebSocketReceiver { events, task: None },
        acknowledged,
        deadline,
    )
}

async fn permission_api() -> (LarkApi, HttpMockServer) {
    let server = HttpMockServer::start(|request| {
        let body = if request.path.ends_with("tenant_access_token/internal") {
            r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#
        } else if request.path == "/open-apis/bot/v3/info" {
            r#"{"code":0,"msg":"ok","bot":{"open_id":"ou-bot"}}"#
        } else if request.path.ends_with("/reply") {
            r#"{"code":0,"msg":"ok","data":{"message_id":"om-reply"}}"#
        } else if request.method == "PATCH"
            && request.path.starts_with("/open-apis/im/v1/messages/")
        {
            r#"{"code":0,"msg":"ok"}"#
        } else {
            panic!("unexpected Lark request {}", request.path);
        };
        MockResponse::json(body)
    })
    .await;
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-permission-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap();
    (api, server)
}

#[test]
fn image_extensions_cover_known_and_unknown_media_types() {
    assert_eq!(LarkChannel::image_extension("image/png"), "png");
    assert_eq!(LarkChannel::image_extension("image/jpeg"), "jpg");
    assert_eq!(LarkChannel::image_extension("image/webp"), "webp");
    assert_eq!(LarkChannel::image_extension("image/gif"), "gif");
    assert_eq!(LarkChannel::image_extension("image/bmp"), "bmp");
    assert_eq!(LarkChannel::image_extension("image/tiff"), "tiff");
    assert_eq!(LarkChannel::image_extension("image/heic"), "heic");
    assert_eq!(
        LarkChannel::image_extension("application/octet-stream"),
        "img"
    );
}

#[tokio::test]
async fn receiver_routes_ignored_interrupt_card_and_message_events() {
    enable_test_logging();
    let mut channel = LarkChannel::with_api(api());
    channel.receiver = Some(event_receiver([
        LarkEvent::Ignore {
            event_type: "ignored".to_string(),
        },
        LarkEvent::Message(message("file")),
        LarkEvent::Message(message("text")),
    ]));
    let task = channel.recv().await.unwrap().unwrap();
    assert_eq!(task.task_id(), "om-message");

    let interrupted = Arc::new(AtomicBool::new(false));
    let callback_interrupted = Arc::clone(&interrupted);
    let registration = channel.interrupts.register(InterruptCallback::new(move || {
        callback_interrupted.store(true, AtomicOrdering::Relaxed);
        true
    }));
    channel.receiver = Some(event_receiver([
        LarkEvent::Interrupt(LarkInterruptEvent {
            id: "evt-interrupt".to_string(),
            user_id: "ou-user".to_string(),
            session_id: "oc-chat".to_string(),
            message_id: "om-card".to_string(),
            callback_id: registration.id().to_string(),
            conversation: None,
        }),
        LarkEvent::CardAction(LarkCardActionEvent {
            id: "evt-action".to_string(),
            user_id: "ou-user".to_string(),
            session_id: "oc-chat".to_string(),
            message_id: "om-card".to_string(),
            command: CommandRequest::new(["agent", "list"]),
            conversation: None,
        }),
    ]));

    let task = channel.recv().await.unwrap().unwrap();
    assert!(interrupted.load(AtomicOrdering::Relaxed));
    assert_eq!(task.task_id(), "evt-action");
    assert_eq!(task.session_id(), "oc-chat");
    assert_eq!(task.input().command().unwrap().path(), &["agent", "list"]);

    channel.receiver = Some(event_receiver([]));
    assert!(channel.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn private_messages_support_text_replies_runs_and_actions() {
    enable_test_logging();
    assert_eq!(
        LarkMessageEvent::normalize_content("file", "raw"),
        ("raw".to_string(), Vec::new())
    );

    let (api, server) = permission_api().await;
    let mut channel = LarkChannel::with_api(api);
    let mut private_message = message("text");
    private_message.chat_type = "p2p".to_string();
    channel.receiver = Some(event_receiver([LarkEvent::Message(private_message)]));
    let task = channel.recv().await.unwrap().unwrap();

    channel
        .reply(&task, ChannelReply::new("private reply"))
        .await
        .unwrap();
    let run = channel
        .open_run(
            &task,
            ChannelRunContext {
                agent: crate::channel::ChannelAgent {
                    name: "codex".to_string(),
                },
                interrupt: Some(InterruptCallback::new(|| true)),
            },
        )
        .await
        .unwrap();
    run.publish(RunEvent::Started {
        run_id: "run-private".to_string(),
    })
    .await
    .unwrap();

    channel.receiver = Some(event_receiver([LarkEvent::CardAction(
        LarkCardActionEvent {
            id: "evt-private-action".to_string(),
            user_id: "ou-user".to_string(),
            session_id: "oc-chat".to_string(),
            message_id: "om-card".to_string(),
            command: CommandRequest::new(["agent", "list"]),
            conversation: None,
        },
    )]));
    assert!(channel.recv().await.unwrap().is_some());

    let requests = server.requests().await;
    assert!(requests.iter().any(|request| {
        request.path == "/open-apis/im/v1/messages/om-message/reply"
            && request.body.contains("private reply")
    }));
    assert!(requests.iter().any(|request| {
        request.path == "/open-apis/im/v1/messages/om-message/reply"
            && request.body.contains("interactive")
    }));
}

#[tokio::test]
async fn receiver_defers_message_acknowledgement_until_daemon_acceptance() {
    let mut channel = LarkChannel::with_api(api());
    let (receiver, mut acknowledged, deadline) =
        acknowledged_event_receiver(LarkEvent::Message(message("text")));
    channel.receiver = Some(receiver);

    let delivery = channel.recv().await.unwrap().unwrap();
    assert_eq!(delivery.task().task_id(), "om-message");
    let (_, receipt) = delivery.into_parts();
    assert_eq!(receipt.deadline(), deadline);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut acknowledged)
            .await
            .is_err()
    );
    receipt.accept();
    assert_eq!(acknowledged.await.unwrap(), 200);
}

#[tokio::test]
async fn receiver_maps_daemon_acceptance_and_retry_to_lark_status() {
    for (accept, expected) in [(true, 200), (false, 500)] {
        let mut channel = LarkChannel::with_api(api());
        let (receiver, acknowledged, _) =
            acknowledged_event_receiver(LarkEvent::Message(message("text")));
        channel.receiver = Some(receiver);

        let delivery = channel.recv().await.unwrap().unwrap();
        let (_, receipt) = delivery.into_parts();
        if accept {
            receipt.accept();
        } else {
            drop(receipt);
        }

        assert_eq!(acknowledged.await.unwrap(), expected);
    }
}

#[tokio::test]
async fn receiver_rejects_ack_when_attachment_normalization_fails() {
    let server = HttpMockServer::start(|request| {
        if request.path.ends_with("tenant_access_token/internal") {
            MockResponse::json(
                r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#,
            )
        } else {
            MockResponse::json("download failed").with_status(503)
        }
    })
    .await;
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-normalization-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap();
    let mut event = message("post");
    event.image_keys = vec!["img-failed".to_string()];
    let mut channel = LarkChannel::with_api(api);
    let (receiver, acknowledged, _) = acknowledged_event_receiver(LarkEvent::Message(event));
    channel.receiver = Some(receiver);

    assert!(channel.recv().await.is_err());
    assert_eq!(acknowledged.await.unwrap(), 500);
}

#[tokio::test]
async fn receiver_acknowledges_permanent_attachment_failures() {
    let server = HttpMockServer::start(|request| {
        if request.path.ends_with("tenant_access_token/internal") {
            MockResponse::json(
                r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#,
            )
        } else {
            MockResponse::json("missing").with_status(404)
        }
    })
    .await;
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-permanent-attachment-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap();
    let mut event = message("post");
    event.image_keys = vec!["img-missing".to_string()];
    let mut channel = LarkChannel::with_api(api);
    let (receiver, acknowledged, _) = acknowledged_event_receiver(LarkEvent::Message(event));
    channel.receiver = Some(receiver);

    assert!(channel.recv().await.is_err());
    assert_eq!(acknowledged.await.unwrap(), 200);
}

#[test]
fn group_session_cache_is_bounded() {
    let mut sessions = GroupSessions::default();
    for index in 0..=GROUP_SESSION_CAPACITY {
        sessions.insert(format!("chat-{index}"), true);
    }

    assert_eq!(sessions.entries.len(), GROUP_SESSION_CAPACITY);
    assert_eq!(sessions.get("chat-0"), None);
    assert_eq!(
        sessions.get(&format!("chat-{GROUP_SESSION_CAPACITY}")),
        Some(true)
    );
}

#[tokio::test]
async fn receiver_silently_discards_unmentioned_denied_lark_messages() {
    let (api, server) = permission_api().await;
    let mut channel = LarkChannel::with_api_and_permission(
        api,
        permission(&["ou-allowed"], &[("oc-chat", false)]),
    );
    let mut denied = message("text");
    denied.sender_id = "ou-denied".to_string();
    let mut allowed = message("text");
    allowed.sender_id = "ou-allowed".to_string();
    channel.receiver = Some(event_receiver([
        LarkEvent::Message(denied),
        LarkEvent::Message(allowed),
    ]));

    let task = channel.recv().await.unwrap().unwrap();

    assert_eq!(task.task_id(), "om-message");
    let requests = server.requests().await;
    assert!(
        !requests
            .iter()
            .any(|request| request.path.ends_with("/reply"))
    );
}

#[tokio::test]
async fn receiver_guides_a_denied_lark_user_who_mentions_the_bot() {
    let (api, server) = permission_api().await;
    let mut channel = LarkChannel::with_api_and_permission(
        api,
        permission(&["ou-allowed"], &[("oc-chat", false)]),
    );
    let mut denied = message("text");
    denied.sender_id = "ou-denied".to_string();
    denied.mention_ids = vec!["ou-bot".to_string()];
    let mut allowed = message("text");
    allowed.sender_id = "ou-allowed".to_string();
    channel.receiver = Some(event_receiver([
        LarkEvent::Message(denied),
        LarkEvent::Message(allowed),
    ]));

    assert!(channel.recv().await.unwrap().is_some());
    let requests = server.requests().await;
    let reply = requests
        .iter()
        .find(|request| request.path.ends_with("/reply"))
        .unwrap();
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["msg_type"], "interactive");
    let card: Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
    let markdown = card["body"]["elements"][0]["content"].as_str().unwrap();
    assert!(markdown.contains("**无权访问此 Channel**"));
    assert!(markdown.contains("- User ID：`ou-denied`"));
    assert!(markdown.contains("```json"));
}

#[tokio::test]
async fn lark_group_mention_requirement_matches_only_the_current_bot() {
    let (api, server) = permission_api().await;
    let mut channel =
        LarkChannel::with_api_and_permission(api, permission(&["ou-user"], &[("oc-chat", true)]));
    let mut without_bot = message("text");
    without_bot.mention_ids = vec!["ou-other".to_string()];
    let mut with_bot = message("text");
    with_bot.mention_ids = vec!["ou-bot".to_string()];
    channel.receiver = Some(event_receiver([
        LarkEvent::Message(without_bot),
        LarkEvent::Message(with_bot),
    ]));

    assert!(channel.recv().await.unwrap().is_some());
    let requests = server.requests().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == "/open-apis/bot/v3/info")
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.path.ends_with("/reply"))
    );
}

#[tokio::test]
async fn lark_actions_check_the_actor_but_do_not_require_a_new_mention() {
    let (api, server) = permission_api().await;
    let mut channel = LarkChannel::with_api_and_permission(
        api,
        permission(&["ou-allowed"], &[("oc-chat", true)]),
    );
    let mut source = message("text");
    source.sender_id = "ou-allowed".to_string();
    source.mention_ids = vec!["ou-bot".to_string()];
    channel.receiver = Some(event_receiver([LarkEvent::Message(source)]));
    assert!(channel.recv().await.unwrap().is_some());

    let interrupted = Arc::new(AtomicBool::new(false));
    let callback_interrupted = Arc::clone(&interrupted);
    let registration = channel.interrupts.register(InterruptCallback::new(move || {
        callback_interrupted.store(true, AtomicOrdering::Relaxed);
        true
    }));
    channel.receiver = Some(event_receiver([
        LarkEvent::Interrupt(LarkInterruptEvent {
            id: "evt-denied".to_string(),
            user_id: "ou-denied".to_string(),
            session_id: "oc-chat".to_string(),
            message_id: "om-card-denied".to_string(),
            callback_id: registration.id().to_string(),
            conversation: None,
        }),
        LarkEvent::CardAction(LarkCardActionEvent {
            id: "evt-allowed".to_string(),
            user_id: "ou-allowed".to_string(),
            session_id: "oc-chat".to_string(),
            message_id: "om-card-allowed".to_string(),
            command: CommandRequest::new(["agent", "list"]),
            conversation: None,
        }),
    ]));

    let task = channel.recv().await.unwrap().unwrap();

    assert_eq!(task.task_id(), "evt-allowed");
    assert_eq!(task.conversation(), Some(LarkConversation::Group));
    assert!(!interrupted.load(AtomicOrdering::Relaxed));
    let cloned = channel.sender().clone();
    assert!(!channel.group_sessions.lock().unwrap().is_empty());
    cloned
        .reply(&task, ChannelReply::new("legacy action reply"))
        .await
        .unwrap();
    let requests = server.requests().await;
    assert!(requests.iter().any(|request| {
        request.path == "/open-apis/im/v1/messages/om-card-denied/reply"
            && request.body.contains("User ID：`ou-denied`")
    }));
    assert!(requests.iter().any(|request| {
        request.method == "PATCH"
            && request.path == "/open-apis/im/v1/messages/om-card-allowed"
            && request.body.contains("legacy action reply")
    }));
}

#[tokio::test]
async fn marked_group_card_actions_survive_channel_reconstruction() {
    let event = LarkEvent::from_lark_event_payload(
        r#"{
            "schema":"2.0",
            "header":{"event_id":"evt-restarted","event_type":"card.action.trigger"},
            "event":{
                "operator":{"open_id":"ou-allowed"},
                "action":{"tag":"button","value":{
                    "agora_conversation":"group",
                    "agora_command":{"path":["agent","list"],"arguments":{}}
                }},
                "context":{"open_message_id":"om-card","open_chat_id":"oc-restarted"}
            }
        }"#,
    )
    .unwrap();
    let (api, _server) = permission_api().await;
    let mut channel = LarkChannel::with_api_and_permission(
        api,
        permission(&["ou-allowed"], &[("oc-restarted", false)]),
    );
    assert!(channel.group_sessions.lock().unwrap().is_empty());
    channel.receiver = Some(event_receiver([event]));

    let task = channel.recv().await.unwrap();

    assert!(task.is_some());
    assert_eq!(task.unwrap().task_id(), "evt-restarted");
}

#[tokio::test]
async fn marked_private_card_actions_survive_channel_reconstruction() {
    let event = LarkEvent::from_lark_event_payload(
        r#"{
            "schema":"2.0",
            "header":{"event_id":"evt-private-restarted","event_type":"card.action.trigger"},
            "event":{
                "operator":{"open_id":"ou-allowed"},
                "action":{"tag":"button","value":{
                    "agora_conversation":"private",
                    "agora_command":{"path":["agent","list"],"arguments":{}}
                }},
                "context":{"open_message_id":"om-card","open_chat_id":"oc-private"}
            }
        }"#,
    )
    .unwrap();
    let (api, _server) = permission_api().await;
    let mut channel = LarkChannel::with_api_and_permission(api, permission(&["ou-allowed"], &[]));
    assert!(channel.group_sessions.lock().unwrap().is_empty());
    channel.receiver = Some(event_receiver([event]));

    let task = channel.recv().await.unwrap();

    assert!(task.is_some());
    assert_eq!(task.unwrap().task_id(), "evt-private-restarted");
}

#[tokio::test]
async fn receiver_propagates_task_and_join_errors() {
    let (sender, events) = mpsc::channel(1);
    drop(sender);
    let mut receiver = LarkWebSocketReceiver {
        events,
        task: Some(tokio::spawn(async { Err(anyhow!("websocket failed")) })),
    };
    assert!(
        receiver
            .next_delivery()
            .await
            .unwrap_err()
            .to_string()
            .contains("websocket failed")
    );

    let (sender, events) = mpsc::channel(1);
    drop(sender);
    let mut receiver = LarkWebSocketReceiver {
        events,
        task: Some(tokio::spawn(async {
            panic!("websocket panicked");
            #[allow(unreachable_code)]
            Ok(())
        })),
    };
    assert!(
        receiver
            .next_delivery()
            .await
            .unwrap_err()
            .to_string()
            .contains("receiver task failed")
    );
}

#[tokio::test]
async fn card_action_tasks_cannot_open_agent_runs() {
    let channel = LarkChannel::with_api(api());
    assert_eq!(channel.name(), "lark-channel-test");
    let task = LarkTask::from_card_action(LarkCardActionEvent {
        id: "evt-action".to_string(),
        user_id: "ou-user".to_string(),
        session_id: "oc-chat".to_string(),
        message_id: "om-card".to_string(),
        command: CommandRequest::new(["agent", "list"]),
        conversation: None,
    });
    let error = channel
        .open_run(
            &task,
            ChannelRunContext {
                agent: crate::channel::ChannelAgent {
                    name: "codex".to_string(),
                },
                interrupt: None,
            },
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("cannot open"));
}

#[tokio::test]
async fn configured_channel_rejects_a_task_from_another_channel_type() {
    use crate::channel::{ConfiguredChannel, ConfiguredTask};
    use crate::config::{ChannelConfig, TelegramChannelConfig};

    let channel = ConfiguredChannel::from_config(ChannelConfig::Telegram(TelegramChannelConfig {
        name: "telegram".to_string(),
        token: "123:secret".to_string(),
        permission: Default::default(),
        proxy: None,
    }))
    .unwrap();
    let task = ConfiguredTask::Lark(LarkTask::from_message(
        message("text"),
        TaskContent::new("hello"),
    ));

    assert_eq!(task.task_id(), "om-message");
    assert_eq!(task.session_id(), "oc-chat");
    assert_eq!(task.input().message().unwrap().text(), "hello");

    let error = channel
        .open_run(
            &task,
            ChannelRunContext {
                agent: crate::channel::ChannelAgent {
                    name: "codex".to_string(),
                },
                interrupt: None,
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "configured channel and task types do not match"
    );

    let error = channel
        .reply(&task, ChannelReply::new("ignored"))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "configured channel and task types do not match"
    );
}

#[tokio::test]
async fn websocket_receiver_reports_background_task_outcomes() {
    let mut receiver = LarkWebSocketReceiver::spawn(LarkChannel::with_api(api()).action_handler());
    receiver.task.as_ref().unwrap().abort();
    assert!(
        receiver
            .next_delivery()
            .await
            .unwrap_err()
            .to_string()
            .contains("receiver task failed")
    );

    let (sender, events) = mpsc::channel(1);
    drop(sender);
    let mut receiver = LarkWebSocketReceiver {
        events,
        task: Some(tokio::spawn(async { Ok(()) })),
    };
    assert!(receiver.next_delivery().await.unwrap().is_none());
    assert!(receiver.task.is_none());

    let (sender, events) = mpsc::channel(1);
    drop(sender);
    let mut receiver = LarkWebSocketReceiver {
        events,
        task: Some(tokio::spawn(async { anyhow::bail!("websocket stopped") })),
    };
    assert!(
        receiver
            .next_delivery()
            .await
            .unwrap_err()
            .to_string()
            .contains("websocket stopped")
    );

    let (sender, events) = mpsc::channel(1);
    drop(sender);
    let task = tokio::spawn(async {
        std::future::pending::<()>().await;
        Ok(())
    });
    task.abort();
    let mut receiver = LarkWebSocketReceiver {
        events,
        task: Some(task),
    };
    assert!(
        receiver
            .next_delivery()
            .await
            .unwrap_err()
            .to_string()
            .contains("receiver task failed")
    );
}

#[tokio::test]
async fn channel_restarts_a_finished_or_failed_websocket_receiver() {
    enable_test_logging();
    for failed in [false, true] {
        let mut channel = LarkChannel::with_api(api());
        let (sender, events) = mpsc::channel(1);
        drop(sender);
        channel.receiver = Some(LarkWebSocketReceiver {
            events,
            task: Some(tokio::spawn(async move {
                if failed {
                    anyhow::bail!("receiver failed");
                }
                Ok(())
            })),
        });
        let result = channel.recv().await;
        assert_eq!(result.is_err(), failed);
        assert!(
            channel.receiver.is_none(),
            "dead receiver was retained indefinitely"
        );
        let restarted = channel.receiver();
        assert!(
            restarted.task.is_some(),
            "next receive did not create a fresh worker"
        );
        restarted.task.as_ref().unwrap().abort();
    }
}

#[tokio::test]
async fn cancelling_receive_does_not_detach_the_worker_being_joined() {
    let (sender, events) = mpsc::channel(1);
    drop(sender);
    let task = tokio::spawn(async { std::future::pending::<Result<()>>().await });
    let worker = task.abort_handle();
    let mut receiver = LarkWebSocketReceiver {
        events,
        task: Some(task),
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.next_delivery())
            .await
            .is_err()
    );
    drop(receiver);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let finished = worker.is_finished();
    worker.abort();
    assert!(
        finished,
        "cancelled recv lost ownership of the websocket worker"
    );
}

#[tokio::test]
async fn leading_bot_mentions_do_not_hide_commands_or_rewrite_the_prompt() {
    let (api, _server) = permission_api().await;
    let mut channel = LarkChannel::with_api_and_permission(api, permission(&["*"], &[("*", true)]));
    for (input, expected) in [
        ("@_user_1 /stop", Some("/stop")),
        (" @_user_1\n/stop agent", Some("/stop agent")),
        (
            "@_user_1 /ask agent \n  keep indentation  ",
            Some("/ask agent \n  keep indentation  "),
        ),
        ("@_user_1 @_user_2 /stop", Some("@_user_1 @_user_2 /stop")),
        (
            "@_user_1 ordinary prompt  ",
            Some("@_user_1 ordinary prompt  "),
        ),
        ("@_user_1", Some("@_user_1")),
        ("@_user_10 /stop", Some("@_user_10 /stop")),
        ("hello @_user_1", Some("hello @_user_1")),
    ] {
        let event = LarkEvent::from_lark_event_payload(serde_json::to_vec(&serde_json::json!({
            "header": {"event_id": "evt-mentioned", "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou-user"}},
                "message": {
                    "message_id": "om-mentioned", "chat_id": "oc-chat", "chat_type": "group",
                    "message_type": "text", "content": serde_json::json!({"text": input}).to_string(),
                    "mentions": [
                        {"key": "@_user_1", "id": {"open_id": "ou-bot"}, "name": "Agora"},
                        {"key": "@_user_2", "id": {"open_id": "ou-other"}, "name": "Other"}
                    ]
                }
            }
        })).unwrap()).unwrap();
        let task = channel.handle_event(event).await.unwrap().unwrap();
        assert_eq!(
            task.input().message().map(TaskContent::text),
            expected,
            "input={input:?}"
        );
    }
}

#[tokio::test]
async fn interrupt_buttons_report_accepted_and_expired_without_stopping_another_run() {
    let (api, server) = permission_api().await;
    let mut channel = LarkChannel::with_api(api);
    let triggered = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&triggered);
    let registration = channel.interrupts.register(InterruptCallback::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        true
    }));
    for id in [registration.id(), registration.id(), "old-daemon-button"] {
        channel
            .handle_event(LarkEvent::Interrupt(LarkInterruptEvent {
                id: "evt-interrupt".into(),
                user_id: "ou-user".into(),
                session_id: "oc-chat".into(),
                message_id: "om-card".into(),
                callback_id: id.into(),
                conversation: Some(LarkConversation::Group),
            }))
            .await
            .unwrap();
    }
    assert_eq!(triggered.load(Ordering::SeqCst), 1);
    let replies = server
        .requests()
        .await
        .into_iter()
        .filter(|request| request.path.ends_with("/reply"))
        .map(|request| request.body)
        .collect::<Vec<_>>();
    assert_eq!(
        replies.len(),
        3,
        "button outcomes were invisible to the user"
    );
    assert!(replies[0].contains("停止请求已受理"));
    assert!(replies[1].contains("已结束或按钮已失效"));
    assert!(replies[2].contains("已结束或按钮已失效"));
}

#[tokio::test]
async fn interrupt_bypasses_slow_image_download_and_full_message_queue() {
    let server = HttpMockServer::start(|request| {
        if request.path.ends_with("tenant_access_token/internal") {
            MockResponse::json(
                r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#,
            )
        } else if request.path.contains("/resources/") {
            MockResponse::bytes(b"image".to_vec(), "image/png").with_delay(Duration::from_secs(2))
        } else {
            MockResponse::json(r#"{"code":0,"msg":"ok","data":{"message_id":"reply"}}"#)
        }
    })
    .await;
    let mut channel = LarkChannel::with_api(
        LarkApi::with_base_url(
            LarkChannelConfig {
                name: "test".into(),
                app_id: "app".into(),
                secret: "secret".into(),
                permission: Default::default(),
                proxy: None,
            },
            server.base_url(),
        )
        .unwrap(),
    );
    let (messages, events) = mpsc::channel(1);
    let sender = LarkEventSender {
        messages,
        actions: Some(Arc::new(channel.action_handler())),
    };
    channel.receiver = Some(LarkWebSocketReceiver { events, task: None });
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let registration = channel.interrupts.register(InterruptCallback::new(move || {
        flag.store(true, AtomicOrdering::SeqCst);
        true
    }));
    let mut image = message("image");
    image.image_keys.push("img".into());
    let image_sender = sender.clone();
    let delivery =
        tokio::spawn(async move { image_sender.deliver(LarkEvent::Message(image)).await });
    let receiving = tokio::spawn(async move { channel.recv().await });
    server.wait_for_method_count("GET", 1).await;
    let (queued, _) = LarkDelivery::new(LarkEvent::Message(message("text")));
    sender.messages.try_send(queued).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        sender.deliver(LarkEvent::Interrupt(LarkInterruptEvent {
            id: "evt-stop".into(),
            user_id: "ou-user".into(),
            session_id: "oc-chat".into(),
            message_id: "om-card".into(),
            callback_id: registration.id().into(),
            conversation: Some(LarkConversation::Group),
        })),
    )
    .await;
    assert_eq!(
        result
            .expect("slow normalization blocked cancellation")
            .unwrap(),
        200
    );
    assert!(interrupted.load(AtomicOrdering::SeqCst));
    assert!(
        !receiving.is_finished(),
        "fixture did not hold image download open"
    );
    let received = receiving.await.unwrap().unwrap().unwrap();
    let (_, receipt) = received.into_parts();
    receipt.accept();
    assert_eq!(delivery.await.unwrap().unwrap(), 200);
}

#[tokio::test]
async fn independent_interrupt_admission_still_checks_permissions() {
    let (api, server) = permission_api().await;
    let channel =
        LarkChannel::with_api_and_permission(api, permission(&["allowed"], &[("oc-chat", true)]));
    let (messages, _events) = mpsc::channel(1);
    let sender = LarkEventSender {
        messages,
        actions: Some(Arc::new(channel.action_handler())),
    };
    let triggered = Arc::new(AtomicBool::new(false));
    let flag = triggered.clone();
    let registration = channel.interrupts.register(InterruptCallback::new(move || {
        flag.store(true, AtomicOrdering::SeqCst);
        true
    }));
    for (user, chat, conversation) in [
        ("denied", "oc-chat", Some(LarkConversation::Group)),
        ("allowed", "denied-chat", Some(LarkConversation::Group)),
        ("allowed", "oc-chat", None),
    ] {
        assert_eq!(
            sender
                .deliver(LarkEvent::Interrupt(LarkInterruptEvent {
                    id: "evt-denied".into(),
                    user_id: user.into(),
                    session_id: chat.into(),
                    message_id: "om-card".into(),
                    callback_id: registration.id().into(),
                    conversation,
                }))
                .await
                .unwrap(),
            200
        );
        assert!(!triggered.load(AtomicOrdering::SeqCst));
    }
    assert_eq!(
        server
            .requests()
            .await
            .iter()
            .filter(|request| request.path.ends_with("/reply"))
            .count(),
        3
    );
    assert!(
        channel.interrupts.trigger(registration.id()),
        "denial consumed another user's callback"
    );
}

#[tokio::test]
async fn interrupt_feedback_failure_does_not_undo_cancellation() {
    let channel = LarkChannel::with_api(api());
    let triggered = Arc::new(AtomicBool::new(false));
    let flag = triggered.clone();
    let registration = channel.interrupts.register(InterruptCallback::new(move || {
        flag.store(true, AtomicOrdering::SeqCst);
        true
    }));
    let (messages, _events) = mpsc::channel(1);
    let sender = LarkEventSender {
        messages,
        actions: Some(Arc::new(channel.action_handler())),
    };
    assert_eq!(
        sender
            .deliver(LarkEvent::Interrupt(LarkInterruptEvent {
                id: "evt-stop".into(),
                user_id: "ou-user".into(),
                session_id: "oc-chat".into(),
                message_id: "om-card".into(),
                callback_id: registration.id().into(),
                conversation: Some(LarkConversation::Group),
            }))
            .await
            .unwrap(),
        200
    );
    assert!(triggered.load(AtomicOrdering::SeqCst));
    assert!(!channel.interrupts.trigger(registration.id()));
}
