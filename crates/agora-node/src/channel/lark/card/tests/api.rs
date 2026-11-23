use super::*;

#[tokio::test]
async fn lark_api_replies_to_commands_without_forcing_a_thread() {
    let server = lark_http_server().await;
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap();
    let token = api.tenant_access_token().await.unwrap();

    api.reply_text(
        &token,
        &LarkReplyTarget {
            message_id: "om_source".to_string(),
            mention_user_id: Some("ou_sender".into()),
        },
        "Stopped 1 agent: codex-dev.",
    )
    .await
    .unwrap();

    let requests = server.requests().await;
    let request = requests
        .iter()
        .find(|request| request.path == "/open-apis/im/v1/messages/om_source/reply")
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    let content: serde_json::Value =
        serde_json::from_str(body["content"].as_str().unwrap()).unwrap();

    assert_eq!(request.method, "POST");
    assert_eq!(body["msg_type"], "text");
    assert_eq!(body["reply_in_thread"], false);
    assert_eq!(
        content["text"],
        "<at user_id=\"ou_sender\"></at> Stopped 1 agent: codex-dev."
    );
}

#[tokio::test]
async fn lark_agent_toggle_action_patches_the_original_status_card() {
    let server = lark_http_server().await;
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap();
    let channel = LarkChannel::with_api(api);
    let task = LarkTask::from_card_action(LarkCardActionEvent {
        id: "evt_action".to_string(),
        user_id: "ou_user".to_string(),
        session_id: "oc_chat".to_string(),
        message_id: "om_status_card".to_string(),
        command: CommandRequest::new(["agent", "enable"]).with_argument("agent_name", "reviewer"),
        conversation: Some(LarkConversation::Group),
    });

    channel
        .reply(
            &task,
            ChannelReply::agent_list(vec![agent_status_with_button("reviewer", true)]),
        )
        .await
        .unwrap();

    let requests = server.requests().await;
    let patch = requests
        .iter()
        .find(|request| {
            request.method == "PATCH" && request.path == "/open-apis/im/v1/messages/om_status_card"
        })
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&patch.body).unwrap();
    let card: serde_json::Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        card.pointer("/body/elements/0/columns/1/elements/0/text/content")
            .unwrap(),
        "Disable"
    );
}

#[tokio::test]
async fn lark_ask_message_quotes_the_source_without_forcing_a_thread() {
    let server = lark_http_server().await;
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap();
    let channel = ConfiguredChannel::Lark(LarkChannel::with_api(api));
    let task = ConfiguredTask::Lark(LarkTask::from_message(
        LarkMessageEvent {
            id: "evt_message".to_string(),
            message_id: "om_ask".to_string(),
            chat_id: "oc_chat".to_string(),
            chat_type: "group".to_string(),
            sender_id: "ou_user".to_string(),
            message_type: "text".to_string(),
            content: "/agent list".to_string(),
            image_keys: Vec::new(),
            mention_ids: Vec::new(),
            mention_keys: Default::default(),
            references: Default::default(),
        },
        crate::task::TaskContent::new("/agent list"),
    ));

    channel
        .reply(
            &task,
            ChannelReply::agent_list(vec![agent_status_with_button("codex-dev", true)]),
        )
        .await
        .unwrap();

    let requests = server.requests().await;
    let reply = requests
        .iter()
        .find(|request| request.path == "/open-apis/im/v1/messages/om_ask/reply")
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["msg_type"], "interactive");
    assert_eq!(body["reply_in_thread"], false);
    let card: serde_json::Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        card.pointer("/header/title/content").unwrap(),
        "当前对话的 Agent 状态"
    );
    assert_eq!(
        card.pointer("/body/elements/0/content").unwrap(),
        "<at id=\"ou_user\"></at>"
    );
    assert_eq!(
        card.pointer("/body/elements/1/columns/1/elements/0/behaviors/0/value/agora_conversation")
            .and_then(serde_json::Value::as_str),
        Some("group")
    );
}
