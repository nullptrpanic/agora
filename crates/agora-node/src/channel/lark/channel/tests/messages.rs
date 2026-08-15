use super::*;

#[test]
fn parses_lark_message_receive_event_payload() {
    let LarkEvent::Message(event) = LarkEvent::from_lark_event_payload(
        r#"{
            "schema": "2.0",
            "header": {
                "event_id": "evt_1",
                "event_type": "im.message.receive_v1",
                "create_time": "1608725989000",
                "tenant_key": "tenant_1"
            },
            "event": {
                "sender": {
                    "sender_id": {
                        "open_id": "ou_123"
                    },
                    "sender_type": "user"
                },
                "message": {
                    "message_id": "om_123",
                    "chat_id": "oc_123",
                    "chat_type": "group",
                    "message_type": "text",
                    "content": "{\"text\":\"@_user_1 run tests\"}",
                    "mentions": [
                        {
                            "key": "@_user_1",
                            "id": {"open_id": "ou_bot"},
                            "name": "Agora"
                        },
                        {
                            "key": "@_user_2",
                            "id": {"open_id": "ou_other"},
                            "name": "Other"
                        }
                    ]
                }
            }
        }"#,
    )
    .unwrap() else {
        panic!("receive event should contain a message");
    };

    assert_eq!(event.id, "evt_1");
    assert_eq!(event.message_id, "om_123");
    assert_eq!(event.session_id(), "oc_123");
    assert_eq!(event.input(), "@_user_1 run tests");
    assert_eq!(event.mention_ids(), &["ou_bot", "ou_other"]);
    assert_eq!(event.reply_target().message_id, "om_123");
}

#[test]
fn parses_lark_post_text_and_image_references() {
    let LarkEvent::Message(event) = LarkEvent::from_lark_event_payload(
        r#"{
            "schema": "2.0",
            "header": {
                "event_id": "evt_post_1",
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "sender": {"sender_id": {"open_id": "ou_123"}},
                "message": {
                    "message_id": "om_post_1",
                    "chat_id": "oc_123",
                    "chat_type": "group",
                    "message_type": "post",
                    "content": "{\"title\":\"\",\"content\":[[{\"tag\":\"img\",\"image_key\":\"img_trace\"}],[{\"tag\":\"text\",\"text\":\"analyze this image\"}]]}"
                }
            }
        }"#,
    )
    .unwrap() else {
        panic!("receive event should contain a message");
    };

    assert_eq!(event.input(), "analyze this image");
    assert_eq!(event.image_keys(), &["img_trace"]);
    assert!(event.is_supported_message());
}

#[test]
fn lark_post_preserves_node_and_paragraph_boundaries() {
    let (text, images) = LarkMessageEvent::flatten_post_content(&serde_json::json!({
        "content": [
            [
                {"tag": "text", "text": "run"},
                {"tag": "text", "text": "tests"}
            ],
            [{"tag": "text", "text": "then report"}]
        ]
    }));

    assert_eq!(text, "run tests\nthen report");
    assert!(images.is_empty());
}

#[test]
fn lark_messages_without_a_sender_identity_are_rejected() {
    let error = LarkEvent::from_lark_event_payload(
        r#"{
            "header": {"event_id": "evt_missing_sender", "event_type": "im.message.receive_v1"},
            "event": {"message": {
                "message_id": "om_missing_sender",
                "chat_id": "oc_123",
                "chat_type": "p2p",
                "message_type": "text",
                "content": "{\"text\":\"hello\"}"
            }}
        }"#,
    )
    .unwrap_err();

    assert!(error.to_string().contains("sender.sender_id"));
}

#[test]
fn accepts_a_standalone_lark_image_message() {
    let LarkEvent::Message(event) = LarkEvent::from_lark_event_payload(
        r#"{
            "schema": "2.0",
            "header": {
                "event_id": "evt_image_1",
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "sender": {"sender_id": {"open_id": "ou_123"}},
                "message": {
                    "message_id": "om_image_1",
                    "chat_id": "oc_123",
                    "chat_type": "group",
                    "message_type": "image",
                    "content": "{\"image_key\":\"img_standalone\"}"
                }
            }
        }"#,
    )
    .unwrap() else {
        panic!("receive event should contain a message");
    };

    assert_eq!(event.input(), "");
    assert_eq!(event.image_keys(), &["img_standalone"]);
    assert!(event.is_supported_message());
}
