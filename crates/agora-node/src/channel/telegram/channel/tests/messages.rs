use super::*;
use serde_json::json;

#[tokio::test]
async fn telegram_acknowledges_an_accepted_stop_only_on_the_original_run() {
    let server =
        HttpMockServer::start(|_| MockResponse::json(r#"{"ok":true,"result":{"message_id":2}}"#))
            .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let channel = TelegramChannel::with_api(api);
    let task = TelegramUpdate::from_json(r#"{"update_id":1,"message":{"message_id":7,"from":{"id":42},"chat":{"id":42,"type":"private"},"text":"/stop"}}"#)
        .unwrap().into_task("agora").unwrap();
    channel
        .reply(
            &task,
            ChannelReply::StopAccepted("已停止 1 个 Agent".into()),
        )
        .await
        .unwrap();
    assert!(server.requests().await.is_empty());
    channel
        .reply(&task, ChannelReply::Text("当前没有运行中的任务".into()))
        .await
        .unwrap();
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[test]
fn telegram_permission_denial_owns_its_rich_markdown_layout() {
    let denial =
        PermissionDenial::new("telegram1", "42", Some("-1001"), "当前群聊未在允许列表中。");

    let markdown = denial.markdown();

    assert!(markdown.starts_with("**无权访问此 Channel**"));
    assert!(markdown.contains("> 当前群聊未在允许列表中。"));
    assert!(markdown.contains("- Channel：`telegram1`"));
    assert!(markdown.contains("- Group ID：`-1001`"));
    assert!(markdown.contains("```jsonc"));
    assert!(markdown.contains(r#""channels": ["#));
    assert!(markdown.contains("// ..."));
    assert!(!markdown.contains(r#""name": "telegram1""#));
}

#[test]
fn telegram_renders_agent_status_replies_without_interactive_controls() {
    let list = ChannelReply::agent_list(vec![
        ChannelAgentStatus::new("codex-dev", true),
        ChannelAgentStatus::new("reviewer", false),
    ]);
    assert_eq!(
        TelegramChannel::render_reply(&list),
        "**当前对话的 Agent 状态**\n> 配置仅对当前对话生效\n\n\
         🟢 **codex-dev** · 已启用\n接收后续消息\n\n\
         ⚪ **reviewer** · 已禁用\n不接收后续消息"
    );

    let status = ChannelReply::agent_status(ChannelAgentStatus::new("reviewer", false));
    assert_eq!(
        TelegramChannel::render_reply(&status),
        "**当前对话的 Agent 状态**\n> 配置仅对当前对话生效\n\n\
         ⚪ **reviewer** · 已禁用\n不接收后续消息"
    );
}

#[test]
fn normalizes_private_text_message() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 101,
            "message": {
                "message_id": 7,
                "from": {"id": 1, "is_bot": false},
                "chat": {"id": 1, "type": "private"},
                "text": "hello"
            }
        }"#,
    )
    .unwrap();

    let task = update.into_task("agora_bot").unwrap();

    assert_eq!(task.task_id(), "101");
    assert_eq!(task.session_id(), "chat:1");
    assert_eq!(task.input().message().unwrap().text(), "hello");
    assert_eq!(task.reply_target().chat_id, 1);
    assert_eq!(task.reply_target().message_id, 7);
    assert_eq!(task.reply_target().message_thread_id, None);
    assert!(task.reply_target().is_private);
    assert_eq!(task.sender_id, "1");
    assert_eq!(task.group_id, None);
    assert!(!task.mentioned_bot);
}

#[test]
fn normalizes_forum_topic_session_and_reply_target() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 102,
            "message": {
                "message_id": 8,
                "message_thread_id": 44,
                "from": {"id": 1, "is_bot": false},
                "chat": {"id": -1001, "type": "supergroup"},
                "text": "run tests"
            }
        }"#,
    )
    .unwrap();

    let task = update.into_task("agora_bot").unwrap();

    assert_eq!(task.task_id(), "102");
    assert_eq!(task.session_id(), "chat:-1001:topic:44");
    assert_eq!(task.input().message().unwrap().text(), "run tests");
    assert_eq!(task.reply_target().chat_id, -1001);
    assert_eq!(task.reply_target().message_id, 8);
    assert_eq!(task.reply_target().message_thread_id, Some(44));
    assert!(!task.reply_target().is_private);
}

#[test]
fn normalizes_photo_caption() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 103,
            "message": {
                "message_id": 9,
                "from": {"id": 1, "is_bot": false},
                "chat": {"id": 1, "type": "private"},
                "caption": "analyze this image",
                "photo": [
                    {"file_id": "photo-small"},
                    {"file_id": "photo-large"}
                ]
            }
        }"#,
    )
    .unwrap();

    let task = update.into_task("agora_bot").unwrap();

    assert_eq!(task.input().message().unwrap().text(), "analyze this image");
}

#[test]
fn accepts_a_photo_without_a_caption() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 104,
            "message": {
                "message_id": 10,
                "from": {"id": 1, "is_bot": false},
                "chat": {"id": 1, "type": "private"},
                "photo": [{"file_id": "photo-1"}]
            }
        }"#,
    )
    .unwrap();

    let task = update.into_task("agora_bot").unwrap();

    assert_eq!(task.input().message().unwrap().text(), "");
}

#[test]
fn ignores_messages_without_text_or_photos() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 104,
            "message": {
                "message_id": 10,
                "from": {"id": 1, "is_bot": false},
                "chat": {"id": 1, "type": "private"},
                "text": "   "
            }
        }"#,
    )
    .unwrap();

    assert!(update.into_task("agora_bot").is_none());
}

#[test]
fn ignores_messages_without_an_authenticated_sender() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 109,
            "message": {
                "message_id": 15,
                "chat": {"id": 1, "type": "private"},
                "text": "hello"
            }
        }"#,
    )
    .unwrap();

    assert!(update.into_task("agora_bot").is_none());
}

#[test]
fn normalizes_commands_addressed_to_this_bot() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 105,
            "message": {
                "message_id": 11,
                "from": {"id": 1, "is_bot": false},
                "chat": {"id": -1001, "type": "group"},
                "text": "/stop@Agora_Bot codex-dev"
            }
        }"#,
    )
    .unwrap();

    let task = update.into_task("agora_bot").unwrap();

    assert_eq!(task.input().message().unwrap().text(), "/stop codex-dev");
    assert!(task.mentioned_bot);
}

#[test]
fn identifies_the_sender_group_and_current_bot_mention() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 107,
            "message": {
                "message_id": 13,
                "from": {"id": 42, "is_bot": false},
                "chat": {"id": -1001, "type": "group"},
                "text": "hello @Agora_Bot"
            }
        }"#,
    )
    .unwrap();

    let task = update.into_task("agora_bot").unwrap();

    assert_eq!(task.sender_id, "42");
    assert_eq!(task.group_id.as_deref(), Some("-1001"));
    assert!(task.mentioned_bot);
}

#[test]
fn ignores_commands_addressed_to_another_bot() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 106,
            "message": {
                "message_id": 12,
                "from": {"id": 1, "is_bot": false},
                "chat": {"id": -1001, "type": "group"},
                "text": "/reset@another_bot"
            }
        }"#,
    )
    .unwrap();

    assert!(update.into_task("agora_bot").is_none());
}

#[test]
fn configured_telegram_channel_is_active() {
    let channel = ConfiguredChannel::from_config(ChannelConfig::Telegram(telegram_config()))
        .expect("telegram channel should be active");

    assert!(matches!(channel, ConfiguredChannel::Telegram(_)));
}

#[test]
fn configured_task_forwards_telegram_task_fields() {
    let update = TelegramUpdate::from_json(
        r#"{
            "update_id": 108,
            "message": {
                "message_id": 14,
                "from": {"id": 7, "is_bot": false},
                "chat": {"id": 7, "type": "private"},
                "text": "hello"
            }
        }"#,
    )
    .unwrap();
    let task = crate::channel::ConfiguredTask::Telegram(update.into_task("agora_bot").unwrap());

    assert_eq!(task.task_id(), "108");
    assert_eq!(task.session_id(), "chat:7");
    assert_eq!(task.input().message().unwrap().text(), "hello");
}

#[tokio::test]
async fn telegram_conversation_context_uses_native_mentions_and_same_topic_quote_id() {
    let server = HttpMockServer::start_json_queue([] as [&str; 0]).await;
    let channel = TelegramChannel::with_api(
        TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap(),
    );
    let quote = json!({"message_id":8,"date":100,"message_thread_id":4,"chat":{"id":-5,"type":"supergroup"},
        "from":{"id":2,"first_name":"小李","username":"li"},"text":"镜像拉取超时"});
    let source = json!({"message_id":10,"date":110,"message_thread_id":4,"chat":{"id":-5,"type":"supergroup"},
        "from":{"id":9},"text":"😀 小李 和 @a_user 的问题是什么原因","entities":[
            {"type":"text_mention","offset":3,"length":2,"user":{"id":2,"first_name":"小李"}},
            {"type":"mention","offset":8,"length":7}
        ],"reply_to_message":quote});
    for same_topic in [true, false] {
        let mut message = source.clone();
        if !same_topic {
            message["reply_to_message"]["message_thread_id"] = json!(99);
        }
        let task = TelegramUpdate::from_value(json!({"update_id":20,"message":message}))
            .unwrap()
            .into_task("agora_bot")
            .unwrap();
        let context = channel.sender().conversation_context(&task).unwrap();
        let mut expected = json!({"channel":"telegram", "chat_type":"group", "chat_id":"-5",
            "message_id":"10", "thread_id":"4",
            "mentions":[{"text":"小李","id":"2"},{"text":"@a_user","id":"@a_user"}]
        });
        if same_topic {
            expected["reply_to_message_id"] = json!("8");
        }
        assert_eq!(serde_json::to_value(context).unwrap(), expected);
    }
    assert!(server.requests().await.is_empty());
}

#[tokio::test]
async fn telegram_private_and_group_use_the_same_metadata_and_quote_only_the_actual_sender() {
    let server = HttpMockServer::start_json_queue([] as [&str; 0]).await;
    let channel = TelegramChannel::with_api(
        TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap(),
    );
    for (chat, kind) in [(9, "private"), (-99, "group")] {
        let event = json!({"update_id":19,"message":{"message_id":79,"chat":{"id":chat,"type":kind},
            "from":{"id":9,"first_name":"小]王"},"date":123,"text":"检查构建"}});
        let task = TelegramUpdate::from_value(event)
            .unwrap()
            .into_task("agora_bot")
            .unwrap();
        let context = channel.conversation_context(&task).unwrap();
        assert_eq!(
            serde_json::to_value(&context).unwrap(),
            json!({"channel":"telegram","chat_type":kind,"chat_id":chat.to_string(),"message_id":"79"})
        );
        assert_eq!(
            task.reply_target.mention.as_deref(),
            (kind == "group").then_some(r"[@小\]王](tg://user?id=9)")
        );
    }
    assert!(server.requests().await.is_empty());
}
