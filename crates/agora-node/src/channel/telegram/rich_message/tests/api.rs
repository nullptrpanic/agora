use super::*;
use std::sync::Arc;

#[tokio::test]
async fn telegram_worker_delivers_updates_received_during_http_without_another_event() {
    let server = HttpMockServer::start(|request| {
        let response = MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#);
        if request.endpoint() == "editMessageText" {
            response.with_delay(Duration::from_millis(100))
        } else {
            response
        }
    })
    .await;
    let message = TelegramRichMessage::with_timing(
        group_target(),
        "agent".into(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::ZERO, Duration::from_secs(3600)),
    );
    message
        .publish(RunEvent::Queued { ahead: 0 })
        .await
        .unwrap();
    message
        .publish(RunEvent::Output(OutputEvent::Answer {
            text: "first".into(),
        }))
        .await
        .unwrap();
    server.wait_for_endpoint_count("editMessageText", 1).await;
    message
        .publish(RunEvent::Output(OutputEvent::Answer {
            text: "second".into(),
        }))
        .await
        .unwrap();
    server.wait_for_endpoint_count("editMessageText", 2).await;
    assert!(
        server
            .requests()
            .await
            .last()
            .unwrap()
            .body
            .contains("firstsecond")
    );
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
}

#[tokio::test]
async fn telegram_slow_delivery_has_one_retry_worker() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::with_timing(
        group_target(),
        "agent".into(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::ZERO, Duration::from_secs(3600)),
    );
    message
        .publish(RunEvent::Queued { ahead: 0 })
        .await
        .unwrap();
    message.inner.state.lock().await.version += 1;
    let delivery = message.inner.delivery_lock.lock().await;
    for _ in 0..32 {
        message
            .handle_flush_failure("test", &anyhow::anyhow!("transport failure"))
            .await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert!(
            Arc::strong_count(&message.inner) <= 2,
            "retries queued extra worker owners"
        );
    }
    drop(delivery);
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
}

#[tokio::test]
async fn telegram_slow_delivery_has_one_update_worker_and_keeps_latest_snapshot() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::with_timing(
        group_target(),
        "agent".into(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::ZERO, Duration::from_secs(3600)),
    );
    message
        .publish(RunEvent::Queued { ahead: 0 })
        .await
        .unwrap();
    let delivery = message.inner.delivery_lock.lock().await;
    for index in 0..32 {
        message
            .publish(RunEvent::Output(OutputEvent::Thinking {
                text: format!("newest-{index}"),
            }))
            .await
            .unwrap();
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert!(
            Arc::strong_count(&message.inner) <= 2,
            "updates queued extra worker owners"
        );
    }
    drop(delivery);
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    assert!(
        server
            .requests()
            .await
            .last()
            .unwrap()
            .body
            .contains("newest-31")
    );
}

#[tokio::test]
async fn multipart_retry_survives_unchanged_primary() {
    let sends = std::sync::atomic::AtomicUsize::new(0);
    let edits = std::sync::atomic::AtomicUsize::new(0);
    let server = HttpMockServer::start(move |request| match request.endpoint() {
        "sendRichMessage" => match sends.fetch_add(1, Ordering::SeqCst) {
            0 => MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#),
            1 => MockResponse::json(
                r#"{"ok":false,"error_code":500,"description":"temporary failure on second part"}"#,
            ),
            _ => MockResponse::json(r#"{"ok":true,"result":{"message_id":101}}"#),
        },
        "editMessageText" => {
            if edits.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#)
            } else {
                MockResponse::json(
                    r#"{"ok":false,"error_code":400,"description":"Bad Request: message is not modified: specified new message content and reply markup are exactly the same as a current content and reply markup of the message"}"#,
                )
                .with_status(400)
            }
        }
        other => panic!("unexpected endpoint {other}"),
    })
    .await;
    let message = TelegramRichMessage::with_timing(
        group_target(),
        "agent".to_string(),
        telegram_api(&server),
        TelegramRichTiming {
            update_interval: Duration::from_secs(3600),
            heartbeat_interval: Duration::from_secs(3600),
            retry_interval: Duration::from_secs(3600),
        },
    );
    message
        .publish(RunEvent::Queued { ahead: 0 })
        .await
        .unwrap();
    message
        .publish(RunEvent::Output(OutputEvent::Answer {
            text: "x".repeat(40_000),
        }))
        .await
        .unwrap();

    assert!(
        message
            .publish(RunEvent::Completed { exit_code: 0 })
            .await
            .is_err()
    );
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();

    assert_eq!(server.endpoint_count("sendRichMessage").await, 3);
    assert!(message.inner.state.lock().await.terminal_sent);
}

#[tokio::test]
async fn default_rich_message_timing_persists_a_complete_private_run() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::new(
        private_target(),
        "codex-dev".to_string(),
        None,
        telegram_api(&server),
    );

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    drop(message);

    server.wait_for_endpoint_count("sendRichMessage", 1).await;
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[tokio::test]
async fn terminal_events_wait_for_delivery_and_return_failures() {
    let server = HttpMockServer::start(|request| match request.endpoint() {
        "sendRichMessageDraft" => MockResponse::json(r#"{"ok":true,"result":true}"#),
        "sendRichMessage" => MockResponse::json(
            r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        )
        .with_status(500),
        method => panic!("unexpected Telegram method {method}"),
    })
    .await;
    let message = TelegramRichMessage::with_timing(
        private_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 1)
        .await;
    let error = message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("sendRichMessage"));
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[tokio::test]
async fn background_flush_does_not_retry_a_non_idempotent_initial_send() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
    ])
    .await;
    let message = TelegramRichMessage::with_timing(
        group_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );

    message
        .publish(RunEvent::Output(OutputEvent::Thinking {
            text: "Inspecting".to_string(),
        }))
        .await
        .unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[tokio::test]
async fn private_run_streams_a_draft_and_persists_one_final_reply() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::with_timing(
        private_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(20), Duration::from_secs(5)),
    );

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 1)
        .await;
    for index in 0..3 {
        message
            .publish(RunEvent::Output(OutputEvent::Thinking {
                text: format!("Thinking {index}"),
            }))
            .await
            .unwrap();
    }
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 2)
        .await;
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 1).await;

    let requests = server.requests().await;
    let drafts = requests
        .iter()
        .filter(|request| request.endpoint() == "sendRichMessageDraft")
        .collect::<Vec<_>>();
    let finals = requests
        .iter()
        .filter(|request| request.endpoint() == "sendRichMessage")
        .collect::<Vec<_>>();
    assert_eq!(drafts.len(), 2);
    assert_eq!(finals.len(), 1);
    let first_draft: serde_json::Value = serde_json::from_str(&drafts[0].body).unwrap();
    let latest_draft: serde_json::Value = serde_json::from_str(&drafts[1].body).unwrap();
    assert_ne!(first_draft["draft_id"], 0);
    assert_eq!(first_draft["draft_id"], latest_draft["draft_id"]);
    assert!(
        latest_draft["rich_message"]["markdown"]
            .as_str()
            .unwrap()
            .contains("Thinking 2")
    );
    let final_body: serde_json::Value = serde_json::from_str(&finals[0].body).unwrap();
    assert_eq!(final_body["chat_id"], 1);
    assert_eq!(final_body["reply_parameters"]["message_id"], 7);
    assert_eq!(final_body["message_thread_id"], 44);
    let final_markdown = final_body["rich_message"]["markdown"].as_str().unwrap();
    assert!(final_markdown.starts_with(
        "**codex-dev** · ✓ 已完成\n\n<details><summary>任务过程 · 3 个阶段</summary>"
    ));
    assert!(final_markdown.contains("**01 · 思考过程**\n\n> ✦ Thinking 0"));
    assert!(final_markdown.contains("**03 · 思考过程**\n\n> ✦ Thinking 2"));
    assert!(final_markdown.ends_with("</details>"));
}

#[tokio::test]
async fn completed_run_sends_as_many_bounded_messages_as_needed() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::with_timing(
        private_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );
    let answer = "long output\n".repeat(800);
    let mut expected = TelegramRichContent::new("codex-dev".to_string());
    expected.apply(RunEvent::Output(OutputEvent::Answer {
        text: answer.clone(),
    }));
    expected.apply(RunEvent::Completed { exit_code: 0 });
    let expected = expected.render_messages(false);
    assert!(expected.len() > 2);

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 1)
        .await;
    message
        .publish(RunEvent::Output(OutputEvent::Answer { text: answer }))
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 2)
        .await;
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessage", expected.len())
        .await;

    let requests = server.requests().await;
    let delivered = requests
        .iter()
        .filter(|request| request.endpoint() == "sendRichMessage")
        .map(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["rich_message"]
                ["markdown"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(delivered, expected);
}

#[tokio::test]
async fn terminal_retry_reuses_multipart_messages_that_were_already_sent() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":true,"result":true}"#,
        r#"{"ok":true,"result":true}"#,
        r#"{"ok":true,"result":{"message_id":100}}"#,
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":true,"result":{"message_id":100}}"#,
        r#"{"ok":true,"result":{"message_id":101}}"#,
    ])
    .await;
    let message = TelegramRichMessage::with_timing(
        private_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );
    let answer = "x".repeat(40_000);
    let mut expected = TelegramRichContent::new("codex-dev".to_string());
    expected.apply(RunEvent::Output(OutputEvent::Answer {
        text: answer.clone(),
    }));
    expected.apply(RunEvent::Completed { exit_code: 0 });
    let expected = expected.render_messages(false);
    assert_eq!(expected.len(), 2);

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 1)
        .await;
    message
        .publish(RunEvent::Output(OutputEvent::Answer { text: answer }))
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 2)
        .await;
    let error = message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Internal Server Error"));
    drop(message);
    server.wait_for_endpoint_count("sendRichMessage", 3).await;
    server.wait_for_endpoint_count("editMessageText", 1).await;

    let requests = server.requests().await;
    let primary_sends = requests
        .iter()
        .filter(|request| request.endpoint() == "sendRichMessage")
        .filter(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["rich_message"]
                ["markdown"]
                == expected[0]
        })
        .count();
    let primary_edits = requests
        .iter()
        .filter(|request| request.endpoint() == "editMessageText")
        .filter(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["rich_message"]
                ["markdown"]
                == expected[0]
        })
        .count();
    assert_eq!(primary_sends, 1);
    assert_eq!(primary_edits, 1);
}

#[tokio::test]
async fn private_run_refreshes_the_draft_until_terminal_state() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::with_timing(
        private_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_millis(25)),
    );

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 2)
        .await;
    message.publish(RunEvent::Stopped).await.unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 1).await;
    let draft_count = server.endpoint_count("sendRichMessageDraft").await;

    tokio::time::sleep(Duration::from_millis(70)).await;

    assert_eq!(
        server.endpoint_count("sendRichMessageDraft").await,
        draft_count
    );
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[tokio::test]
async fn private_run_reports_a_failed_final_reply_without_background_retry() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":true,"result":true}"#,
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":true,"result":{"message_id":100}}"#,
    ])
    .await;
    let message = TelegramRichMessage::with_timing(
        private_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    server
        .wait_for_endpoint_count("sendRichMessageDraft", 1)
        .await;
    let error = message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Internal Server Error"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);

    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    assert_eq!(server.endpoint_count("sendRichMessage").await, 2);
}

#[tokio::test]
async fn group_run_sends_once_and_edits_the_same_topic_message() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::with_timing(
        group_target(),
        "codex-dev".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(20), Duration::from_secs(5)),
    );

    message
        .publish(RunEvent::Started {
            run_id: "run-1".to_string(),
        })
        .await
        .unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 1).await;
    message
        .publish(RunEvent::Output(OutputEvent::Thinking {
            text: "Inspecting".to_string(),
        }))
        .await
        .unwrap();
    server.wait_for_endpoint_count("editMessageText", 1).await;
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    server.wait_for_endpoint_count("editMessageText", 2).await;

    let requests = server.requests().await;
    let sends = requests
        .iter()
        .filter(|request| request.endpoint() == "sendRichMessage")
        .collect::<Vec<_>>();
    let edits = requests
        .iter()
        .filter(|request| request.endpoint() == "editMessageText")
        .collect::<Vec<_>>();
    assert_eq!(sends.len(), 1);
    assert_eq!(edits.len(), 2);
    let send: serde_json::Value = serde_json::from_str(&sends[0].body).unwrap();
    assert_eq!(send["chat_id"], -1001);
    assert_eq!(send["message_thread_id"], 44);
    assert_eq!(send["reply_parameters"]["message_id"], 12);
    for edit in edits {
        let body: serde_json::Value = serde_json::from_str(&edit.body).unwrap();
        assert_eq!(body["chat_id"], -1001);
        assert_eq!(body["message_id"], 100);
    }
}

#[tokio::test]
async fn subscribed_agents_keep_independent_telegram_messages() {
    let server = rich_message_server().await;
    let first = TelegramRichMessage::with_timing(
        group_target(),
        "codex-a".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );
    let second = TelegramRichMessage::with_timing(
        group_target(),
        "codex-b".to_string(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );

    first
        .publish(RunEvent::Started {
            run_id: "run-a".to_string(),
        })
        .await
        .unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 1).await;
    second
        .publish(RunEvent::Started {
            run_id: "run-b".to_string(),
        })
        .await
        .unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 2).await;
    first
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    server.wait_for_endpoint_count("editMessageText", 1).await;
    second
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    server.wait_for_endpoint_count("editMessageText", 2).await;

    let requests = server.requests().await;
    let edited_ids = requests
        .iter()
        .filter(|request| request.endpoint() == "editMessageText")
        .map(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["message_id"]
                .as_i64()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(edited_ids, vec![100, 101]);
}
