use super::*;

#[test]
fn oversized_management_reply_has_a_bounded_explicit_command_fallback() {
    let reply = ChannelReply::agent_list(
        (0..100)
            .map(|index| {
                agent_status_with_button(&format!("{index}-{}", "agent".repeat(100)), true)
            })
            .collect(),
    );
    let card = LarkReplyCard::build(&reply, LarkConversation::Private);
    assert!(card.to_string().len() <= super::super::super::lark_api::LARK_CARD_MAX_BYTES);
    assert!(card.to_string().contains("/agent status"));
    assert!(card.to_string().contains("/agent enable"));
}

#[test]
fn lark_agent_list_card_renders_one_right_aligned_toggle_button_per_agent() {
    let reply = ChannelReply::agent_list(vec![
        agent_status_with_button("codex-dev", true),
        agent_status_with_button("reviewer", false),
    ]);

    let card = LarkReplyCard::build(&reply, LarkConversation::Private);
    assert_eq!(
        card.pointer("/header/title/content").unwrap(),
        "当前对话的 Agent 状态"
    );
    assert_eq!(
        card.pointer("/header/subtitle/content").unwrap(),
        "当前对话 · 2 个 Agent"
    );
    let rows = card
        .pointer("/body/elements")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    let first_button = rows[0].pointer("/columns/1/elements/0").unwrap();
    let second_button = rows[2].pointer("/columns/1/elements/0").unwrap();
    assert_eq!(first_button.pointer("/text/content").unwrap(), "Disable");
    assert_eq!(first_button["type"], "default");
    assert_eq!(
        first_button.pointer("/behaviors/0/value").unwrap(),
        &serde_json::json!({
            "agora_command": {
                "path": ["agent", "disable"],
                "arguments": { "agent_name": "codex-dev" }
            },
            "agora_conversation": "private"
        })
    );
    assert_eq!(second_button.pointer("/text/content").unwrap(), "Enable");
    assert_eq!(second_button["type"], "primary");
    assert_eq!(
        second_button.pointer("/behaviors/0/value").unwrap(),
        &serde_json::json!({
            "agora_command": {
                "path": ["agent", "enable"],
                "arguments": { "agent_name": "reviewer" }
            },
            "agora_conversation": "private"
        })
    );
    let rendered = serde_json::to_string(&card).unwrap();
    assert!(rendered.contains("已启用</font> · 接收后续消息"));
    assert!(rendered.contains("已禁用</font> · 不接收后续消息"));
    assert!(rendered.contains("配置仅对当前对话生效"));
}

#[test]
fn lark_agent_status_card_is_compact_and_has_no_toggle_button() {
    let reply = ChannelReply::agent_status(ChannelAgentStatus::new("reviewer", false));

    let card = LarkReplyCard::build(&reply, LarkConversation::Private);
    let rendered = serde_json::to_string(&card).unwrap();
    assert_eq!(
        card.pointer("/header/subtitle/content").unwrap(),
        "当前对话"
    );
    assert!(rendered.contains("**reviewer**"));
    assert!(rendered.contains("已禁用"));
    assert_eq!(
        card.pointer("/body/elements/0/columns/1/elements/0/text_align")
            .unwrap(),
        "right"
    );
    assert!(!rendered.contains("set_agent_enabled"));
    assert!(!rendered.contains("\"tag\":\"button\""));
}

#[test]
fn lark_reply_card_supports_plain_text_and_danger_actions() {
    let text = LarkReplyCard::build(
        &ChannelReply::Text("plain reply".to_string()),
        LarkConversation::Private,
    );
    assert_eq!(
        text.pointer("/body/elements/0/content")
            .and_then(serde_json::Value::as_str),
        Some("plain reply")
    );

    let danger = ChannelButton::new(
        "Delete",
        ChannelButtonStyle::Danger,
        CommandRequest::new(["delete"]),
    );
    assert_eq!(
        LarkReplyCard::button(&danger, LarkConversation::Private)["type"],
        "danger"
    );
}
