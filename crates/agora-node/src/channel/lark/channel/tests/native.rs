use super::*;
use crate::channel::test_http::{HttpMockServer, MockResponse};
use crate::channel::{ChannelAgent, ChannelRunContext};
use crate::task::{OutputEvent, ProgressStatus, TokenUsage};

#[tokio::test]
async fn native_run_quotes_the_source_without_forcing_a_thread() {
    assert_native_run("group").await;
    assert_native_run("p2p").await;
}

async fn assert_native_run(chat_type: &str) {
    let server = HttpMockServer::start(|request| {
        MockResponse::json(if request.endpoint() == "internal" {
            r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#
        } else if request.path.contains("message_cot?") {
            r#"{"code":0,"msg":"ok","data":{"cot_id":"cot","message_id":"process"}}"#
        } else {
            r#"{"code":0,"msg":"ok","data":{"message_id":"answer"}}"#
        })
    })
    .await;
    let channel = LarkChannel::with_api(
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
    let task = LarkTask::from_message(
        LarkMessageEvent {
            id: "event".into(),
            message_id: "source".into(),
            chat_id: "chat".into(),
            chat_type: chat_type.into(),
            sender_id: "user".into(),
            message_type: "text".into(),
            content: "test".into(),
            image_keys: vec![],
            mention_ids: vec![],
            mention_keys: Default::default(),
            references: Default::default(),
        },
        TaskContent::new("test"),
    );
    let run = channel
        .open_run(
            &task,
            ChannelRunContext {
                agent: ChannelAgent {
                    name: "agent".into(),
                },
                interrupt: None,
            },
        )
        .await
        .unwrap();
    run.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    run.publish(RunEvent::Output(OutputEvent::Thinking {
        text: "**Checking URL formatting**".into(),
    }))
    .await
    .unwrap();
    run.publish(RunEvent::Output(OutputEvent::Commentary {
        id: "c".into(),
        text: "先检查配置。".into(),
    }))
    .await
    .unwrap();
    for status in [ProgressStatus::Running, ProgressStatus::Completed] {
        run.publish(RunEvent::Output(OutputEvent::CommandExecution {
            id: "tool".into(),
            command: "cargo test".into(),
            status,
            exit_code: Some(0),
        }))
        .await
        .unwrap();
    }
    run.publish(RunEvent::Output(OutputEvent::Answer {
        text: "验证通过。".into(),
    }))
    .await
    .unwrap();
    run.publish(RunEvent::Output(OutputEvent::Usage(TokenUsage {
        input_tokens: 98765,
        cached_input_tokens: 0,
        output_tokens: 12345,
        reasoning_output_tokens: 0,
    })))
    .await
    .unwrap();
    run.publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    run.publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    let requests = server.requests().await;
    let creates = requests
        .iter()
        .filter(|r| r.path.contains("message_cot?"))
        .collect::<Vec<_>>();
    assert_eq!(
        creates.len(),
        1,
        "agent execution must use a native COT process message"
    );
    let create: Value = serde_json::from_str(&creates[0].body).unwrap();
    assert_eq!(create["receive_id"], "chat");
    assert_eq!(create["origin_message_id"], "source");
    assert_eq!(create["reply_in_thread"], false);
    let answers = requests
        .iter()
        .filter(|r| r.path.ends_with("/reply"))
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 1);
    let answer: Value = serde_json::from_str(&answers[0].body).unwrap();
    assert_eq!(answers[0].path, "/open-apis/im/v1/messages/source/reply");
    assert_eq!(answer["msg_type"], "post");
    assert_eq!(answer["reply_in_thread"], false);
    assert!(answer["uuid"].as_str().is_some_and(|v| !v.is_empty()));
    let post: Value = serde_json::from_str(answer["content"].as_str().unwrap()).unwrap();
    if chat_type == "group" {
        assert_eq!(
            post["zh_cn"]["content"][0][0],
            serde_json::json!({"tag":"at", "user_id":"user"})
        );
        assert_eq!(post["zh_cn"]["content"][1][0]["text"], "验证通过。");
    } else {
        assert_eq!(
            post["zh_cn"]["content"],
            serde_json::json!([[{"tag":"md", "text":"验证通过。"}]])
        );
    }
    let events = requests
        .iter()
        .filter(|r| r.method == "PUT" && r.path.ends_with("message_cot"))
        .flat_map(|r| {
            serde_json::from_str::<Value>(&r.body).unwrap()["events"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event_type"] == "TOOL_CALL_START")
            .count(),
        1
    );
    assert!(events.iter().any(|e| e["event_type"] == "RUN_FINISHED"));
    assert!(
        events
            .iter()
            .all(|e| e["content"].is_string() && e["timestamp"].is_string())
    );
    let process = serde_json::to_string(&events).unwrap();
    assert!(!process.contains("REASONING_MESSAGE_"));
    assert!(
        !requests
            .iter()
            .any(|r| r.body.contains("Checking URL formatting"))
    );
    assert!(process.contains("先检查配置"));
    assert!(!process.contains("验证通过"));
    assert!(!process.contains("98765"));
    assert!(!requests.iter().any(|r| r.body.contains("interactive")));
}

#[tokio::test]
async fn redelivery_reuses_the_fallback_post_after_an_unconfirmed_process_create() {
    let posts = AtomicUsize::new(0);
    let server = HttpMockServer::start(move |r| {
        if r.endpoint() == "internal" {
            MockResponse::json(
                r#"{"code":0,"tenant_access_token":"token","expire":7200,"msg":"ok"}"#,
            )
        } else if r.path.contains("message_cot?") {
            MockResponse::json(r#"{"code":0,"data":{"cot_id":"cut"#)
        } else if r.endpoint() == "reply" && posts.fetch_add(1, Ordering::SeqCst) < 3 {
            MockResponse::json("{}").with_status(503)
        } else {
            MockResponse::json(r#"{"code":0,"msg":"ok","data":{"message_id":"fallback"}}"#)
        }
    })
    .await;
    let channel = LarkChannel::with_api(
        LarkApi::with_base_url(
            LarkChannelConfig {
                name: "lark".into(),
                app_id: "app".into(),
                secret: "secret".into(),
                permission: Default::default(),
                proxy: None,
            },
            server.base_url(),
        )
        .unwrap(),
    );
    let mut event = super::channel::message("text");
    event.chat_type = "p2p".into();
    let task = LarkTask::from_message(event, TaskContent::new("test"));
    let context = || ChannelRunContext {
        agent: ChannelAgent {
            name: "agent".into(),
        },
        interrupt: None,
    };
    let first = channel.open_run(&task, context()).await.unwrap();
    assert!(first.publish(RunEvent::Queued { ahead: 0 }).await.is_err());
    drop(first);
    let retried = channel.open_run(&task, context()).await.unwrap();
    retried
        .publish(RunEvent::Queued { ahead: 0 })
        .await
        .unwrap();
    retried
        .publish(RunEvent::Started {
            run_id: "run".into(),
        })
        .await
        .unwrap();
    retried
        .publish(RunEvent::Output(OutputEvent::Answer { text: "ok".into() }))
        .await
        .unwrap();
    retried
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    let requests = server.requests().await;
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.contains("message_cot?"))
            .count(),
        1
    );
    let replies: Vec<_> = requests
        .iter()
        .filter(|r| r.endpoint() == "reply")
        .map(|r| &r.body)
        .collect();
    assert_eq!(replies.len(), 4);
    assert!(replies.iter().all(|r| *r == replies[0]));
    assert!(requests.iter().any(|r| r.method == "PUT"
        && r.path.ends_with("/messages/fallback")
        && r.body.contains("ok")));
}
