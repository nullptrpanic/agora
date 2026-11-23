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
            .publish(RunEvent::Output(OutputEvent::Commentary {
                id: "progress".into(),
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
        .publish(RunEvent::Output(OutputEvent::Commentary {
            id: "inspect".into(),
            text: "Inspecting".to_string(),
        }))
        .await
        .unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[tokio::test]
async fn private_reply_shows_two_curl_actions_in_their_commentary_steps() {
    let server = rich_message_server().await;
    let message = TelegramRichMessage::with_timing(
        private_target(),
        "agent".into(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::from_millis(5), Duration::from_secs(5)),
    );
    let first = "/bin/bash -lc 'timeout 15s curl https qq'";
    let retry = r#"/bin/bash -lc "curl -sS --max-time 12 -o /dev/null -w 'HTTP %{http_code}\nProxy CONNECT %{http_connect}' https://qq.com""#;
    for (id, commentary, command, output) in [
        (
            "first",
            "我先按原命令执行。",
            first,
            "503 Forwarding failure",
        ),
        ("retry", "我用完整网址再试一次。", retry, "HTTP 302"),
    ] {
        for event in [
            OutputEvent::Commentary {
                id: format!("{id}-note"),
                text: commentary.into(),
            },
            OutputEvent::CommandExecution {
                id: id.into(),
                command: command.into(),
                status: ProgressStatus::Completed,
                exit_code: Some(0),
            },
            OutputEvent::ToolOutput {
                id: id.into(),
                text: output.into(),
                elapsed_ms: Some(250),
            },
        ] {
            message.publish(RunEvent::Output(event)).await.unwrap();
        }
    }
    message
        .publish(RunEvent::Output(OutputEvent::Answer {
            text: "完整网址返回 HTTP 302。".into(),
        }))
        .await
        .unwrap();
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();

    let requests = server.requests().await;
    let mut final_replies = requests
        .iter()
        .filter(|r| r.endpoint() == "sendRichMessage");
    let body: serde_json::Value =
        serde_json::from_str(&final_replies.next().unwrap().body).unwrap();
    assert!(final_replies.next().is_none());
    assert_eq!(body["reply_parameters"]["message_id"], 7);
    let markdown = body["rich_message"]["markdown"].as_str().unwrap();
    assert!(markdown.starts_with("完整网址返回 HTTP 302。\n\n<details><summary>已完成</summary>"));
    assert!(!markdown.contains("\n\nagent\n") && !markdown.contains("已调用 1 次工具"));
    let mut rest = markdown;
    for (note, title, command, output) in [
        (
            "我先按原命令执行。",
            "运行 curl",
            first,
            "503 Forwarding failure",
        ),
        ("我用完整网址再试一次。", "请求 qq.com", retry, "HTTP 302"),
    ] {
        let row = format!(
            "{note}\n\n<details><summary>{title}</summary>\n\n<pre><code class=\"language-bash\">$ {command}\n\n{output}\n\n✓ Success · 约 0.250 秒</code></pre>"
        );
        rest = rest
            .split_once(&row)
            .expect("action should appear in its own step")
            .1;
    }
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
    message
        .publish(RunEvent::Output(OutputEvent::Thinking {
            text: "hidden summary".into(),
        }))
        .await
        .unwrap();
    for index in 0..3 {
        message
            .publish(RunEvent::Output(OutputEvent::Commentary {
                id: format!("step-{index}"),
                text: format!("Step {index}"),
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
            .contains("Step 2")
    );
    let final_body: serde_json::Value = serde_json::from_str(&finals[0].body).unwrap();
    assert_eq!(final_body["chat_id"], 1);
    assert_eq!(final_body["reply_parameters"]["message_id"], 7);
    assert_eq!(final_body["message_thread_id"], 44);
    let final_markdown = final_body["rich_message"]["markdown"].as_str().unwrap();
    assert!(final_markdown.starts_with("<details><summary>已完成</summary>"));
    assert!(final_markdown.contains("Step 0"));
    assert!(final_markdown.contains("Step 2"));
    assert!(
        !requests
            .iter()
            .any(|request| request.body.contains("hidden summary"))
    );
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
    let expected = expected.render_messages(false, None);
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
    let expected = expected.render_messages(false, None);
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
        .publish(RunEvent::Output(OutputEvent::Commentary {
            id: "inspect".into(),
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

#[tokio::test]
async fn ambiguous_multipart_send_is_never_repeated() {
    let sends = std::sync::atomic::AtomicUsize::new(0);
    let server = HttpMockServer::start(move |request| match request.endpoint() {
        "sendRichMessage" => match sends.fetch_add(1, Ordering::SeqCst) {
            0 => MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#),
            1 => MockResponse::json(r#"{"ok":true,"result":{"message_id":"#),
            _ => MockResponse::json(r#"{"ok":true,"result":{"message_id":102}}"#),
        },
        "editMessageText" => MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#),
        other => panic!("unexpected endpoint {other}"),
    })
    .await;
    let message = TelegramRichMessage::with_timing(
        group_target(),
        "review".into(),
        telegram_api(&server),
        TelegramRichTiming {
            update_interval: Duration::from_secs(3600),
            heartbeat_interval: Duration::from_secs(3600),
            retry_interval: Duration::from_millis(10),
        },
    );
    message
        .publish(RunEvent::Queued { ahead: 0 })
        .await
        .unwrap();
    message
        .publish(RunEvent::Output(OutputEvent::Answer {
            text: "x".repeat(40000),
        }))
        .await
        .unwrap();
    assert!(
        message
            .publish(RunEvent::Completed { exit_code: 0 })
            .await
            .is_err()
    );
    // A later flush and the automatic retry must both respect the unknown receipt.
    let _ = message.publish(RunEvent::Completed { exit_code: 0 }).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(server.endpoint_count("sendRichMessage").await, 2);
}
