use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn private_replies_and_answer_edits_keep_the_quote_without_adding_mentions() {
    let server = HttpMockServer::start(response).await;
    let api = api(&server);
    let target = LarkReplyTarget {
        message_id: "private-source".into(),
        mention_user_id: None,
    };
    let card = json!({"schema":"2.0", "body":{"elements":[{"tag":"markdown", "content":"body"}]}});
    let answer_id = api.reply_post(&target, "body", "uuid").await.unwrap();
    api.update_post(&answer_id, "body", &target).await.unwrap();
    api.reply_text("token", &target, "body").await.unwrap();
    api.reply_card("token", &target, &card).await.unwrap();
    let requests = server.requests().await;
    let replies: Vec<_> = requests
        .iter()
        .filter(|r| r.path.contains("/messages/"))
        .collect();
    assert_eq!(replies.len(), 4);
    for request in replies {
        let body = body(request);
        let content: Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        match body["msg_type"].as_str().unwrap() {
            "post" => assert_eq!(
                content["zh_cn"]["content"],
                json!([[{"tag":"md", "text":"body"}]])
            ),
            "text" => assert_eq!(content, json!({"text":"body"})),
            "interactive" => assert_eq!(content, card),
            _ => panic!("unexpected reply type"),
        }
        if request.method == "POST" {
            assert_eq!(
                request.path,
                "/open-apis/im/v1/messages/private-source/reply"
            );
            assert_eq!(body["reply_in_thread"], false);
        }
    }
}

#[tokio::test]
async fn native_replies_quote_each_source_and_preserve_its_sender_on_edits() {
    let server = HttpMockServer::start(response).await;
    let api = api(&server);
    let first = LarkReplyTarget {
        message_id: "source-a".into(),
        mention_user_id: Some("ou_alice".into()),
    };
    let second = LarkReplyTarget {
        message_id: "source-b".into(),
        mention_user_id: Some("on_bob".into()),
    };
    let (a, b) = tokio::join!(
        api.reply_post(&first, "回答 A", "uuid-a"),
        api.reply_post(&second, "回答 B", "uuid-b"),
    );
    a.unwrap();
    b.unwrap();
    api.update_post("answer-a", &"\"\\🦀".repeat(20_000), &first)
        .await
        .unwrap();
    let requests = server.requests().await;
    assert!(requests.iter().any(|r| r.path.ends_with("/source-a/reply")));
    assert!(requests.iter().any(|r| r.path.ends_with("/source-b/reply")));
    for request in requests.iter().filter(|r| r.path.contains("/messages/")) {
        let body = body(request);
        let post: Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        let expected = if request.path.ends_with("/source-b/reply") {
            "on_bob"
        } else {
            "ou_alice"
        };
        assert_eq!(
            post["zh_cn"]["content"][0][0],
            json!({"tag":"at", "user_id":expected})
        );
        assert_eq!(post["zh_cn"]["content"].as_array().unwrap().len(), 2);
        assert!(request.body.len() <= 30_000);
        if request.method == "POST" {
            assert!(matches!(
                request.path.as_str(),
                "/open-apis/im/v1/messages/source-a/reply"
                    | "/open-apis/im/v1/messages/source-b/reply"
            ));
            assert_eq!(body["reply_in_thread"], false);
        } else {
            assert_eq!(request.path, "/open-apis/im/v1/messages/answer-a");
            assert!(body.get("reply_in_thread").is_none());
            assert!(
                post["zh_cn"]["content"][1][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("[输出已截断]")
            );
        }
    }
}

#[tokio::test]
async fn reply_mentions_cannot_broadcast_or_inject_markup_from_a_sender_id() {
    let server = HttpMockServer::start(response).await;
    let api = api(&server);
    for sender_id in [String::new(), "all".into(), " all ".into(), "x".repeat(513)] {
        let target = LarkReplyTarget {
            message_id: "source".into(),
            mention_user_id: Some(sender_id),
        };
        assert!(api.reply_post(&target, "body", "uuid").await.is_err());
        assert!(api.reply_text("token", &target, "body").await.is_err());
    }
    assert!(server.requests().await.is_empty());
    let target = LarkReplyTarget {
        message_id: "source".into(),
        mention_user_id: Some("ou_\"><at user_id=\"all\">&".into()),
    };
    api.reply_text("token", &target, "body").await.unwrap();
    let requests = server.requests().await;
    let content: Value =
        serde_json::from_str(body(&requests[0])["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        content["text"],
        "<at user_id=\"ou_&quot;&gt;&lt;at user_id=&quot;all&quot;&gt;&amp;\"></at> body"
    );
}

#[tokio::test]
async fn oversized_native_events_are_rejected_before_authentication_or_delivery() {
    let server = HttpMockServer::start(response).await;
    let api = api(&server);
    let cot = CotRef {
        cot_id: "cot".into(),
        message_id: "process".into(),
    };
    let event =
        json!({"event_type":"TOOL_CALL_RESULT", "content":"中".repeat(2000), "timestamp":"1"});
    assert!(
        api.append_cot(&cot, &[event])
            .await
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
    assert!(server.requests().await.is_empty());
}

#[tokio::test]
async fn native_event_retries_preserve_the_exact_deduplication_payload() {
    let attempts = AtomicUsize::new(0);
    let server = HttpMockServer::start(move |request| {
        if request.method == "PUT" && attempts.fetch_add(1, Ordering::SeqCst) < 2 {
            MockResponse::json("{}").with_status(503)
        } else {
            response(request)
        }
    })
    .await;
    let api = api(&server);
    let cot = api.create_cot("chat", "source").await.unwrap();
    let events = vec![
        json!({"event_type":"RUN_STARTED", "content":"{\"runId\":\"r\"}", "timestamp":"1700000000"}),
    ];
    api.append_cot(&cot, &events).await.unwrap();
    let requests = server.requests().await;
    let updates: Vec<_> = requests.iter().filter(|r| r.method == "PUT").collect();
    assert_eq!(updates.len(), 3);
    assert!(updates.windows(2).all(|pair| pair[0].body == pair[1].body));
    assert_eq!(body(updates[0])["events"], json!(events));
    assert_eq!(server.endpoint_count("internal").await, 1);
}

#[tokio::test]
async fn native_auth_refreshes_once_and_permission_errors_do_not_retry() {
    let tokens = AtomicUsize::new(0);
    let server = HttpMockServer::start(move |request| {
        if request.endpoint() == "internal" {
            let n = tokens.fetch_add(1, Ordering::SeqCst) + 1;
            return MockResponse::json(format!(
                r#"{{"code":0,"msg":"ok","tenant_access_token":"token{n}","expire":7200}}"#
            ));
        }
        if request.header("authorization") == Some("Bearer token1") {
            return MockResponse::json(r#"{"code":99991663,"msg":"expired"}"#);
        }
        if request.method == "PUT" {
            return MockResponse::json(r#"{"code":99991672,"msg":"permission denied"}"#);
        }
        response(request)
    })
    .await;
    let api = api(&server);
    let cot = api.create_cot("chat", "source").await.unwrap();
    let error = api
        .append_cot(&cot, &[json!({"event_type":"RUN_STARTED"})])
        .await
        .unwrap_err();
    assert!(!retryable(&error));
    assert_eq!(server.endpoint_count("internal").await, 2);
    assert_eq!(
        server
            .requests()
            .await
            .iter()
            .filter(|r| r.method == "PUT")
            .count(),
        1
    );
}

#[tokio::test]
async fn native_create_is_not_retried_after_an_ambiguous_server_failure() {
    let server = HttpMockServer::start(|request| {
        if request.path.contains("message_cot?") {
            MockResponse::json("{}").with_status(503)
        } else {
            response(request)
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    let _ = reply
        .publish(RunEvent::Failed {
            message: "initialization failed".into(),
        })
        .await;
    let requests = server.requests().await;
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.contains("message_cot?"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.ends_with("/reply"))
            .count(),
        1
    );
}

#[tokio::test]
async fn escaped_long_failed_answer_keeps_its_failure_notice_and_fits_the_wire_limit() {
    let server = HttpMockServer::start(response).await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    reply
        .publish(answer(&format!(
            "{}最新尾部",
            "\u{0001}\n\"🦀".repeat(6000)
        )))
        .await
        .unwrap();
    reply
        .publish(RunEvent::Failed {
            message: "timeout".into(),
        })
        .await
        .unwrap();
    let requests = server.requests().await;
    let post = requests
        .iter()
        .find(|r| r.path.ends_with("/reply"))
        .unwrap();
    assert!(post.body.len() <= 30_000);
    let text = post_text(post);
    assert!(text.starts_with("**任务失败**"));
    assert!(text.contains("部分回答"));
    assert!(text.contains("输出已截断"));
    assert!(text.ends_with("最新尾部"));
}
