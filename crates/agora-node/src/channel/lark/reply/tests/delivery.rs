use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[tokio::test]
async fn stopped_run_only_updates_its_process_without_sending_or_editing_an_answer() {
    for early_answer in [false, true] {
        let server = HttpMockServer::start(response).await;
        let reply = reply(&server);
        reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
        reply.publish(answer("已有回答")).await.unwrap();
        if early_answer {
            reply.flush_answer().await.unwrap();
        }
        reply.publish(answer("尚未发送的内容")).await.unwrap();
        reply.publish(RunEvent::Stopped).await.unwrap();
        reply.publish(RunEvent::Stopped).await.unwrap();
        let requests = server.requests().await;
        assert_eq!(
            server.endpoint_count("reply").await,
            usize::from(early_answer)
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.method == "PUT" && r.path.contains("/messages/"))
        );
        let events = events(&requests);
        let finished: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "RUN_FINISHED")
            .map(event_content)
            .collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(
            finished[0]["input"]["statusText"]["interrupted"]["zh_cn"],
            "已停止"
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.body.contains("已有输出已保留") || r.body.contains("尚未发送的内容"))
        );
    }
}

#[tokio::test]
async fn streaming_answer_is_sent_once_at_completion_and_late_events_are_ignored() {
    let server = HttpMockServer::start(response).await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    for _ in 0..25 {
        reply.publish(answer("正文")).await.unwrap();
        reply.flush_latest().await.unwrap();
    }
    assert_eq!(server.endpoint_count("reply").await, 0);
    reply
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    reply.publish(answer("终态后丢弃")).await.unwrap();
    reply.publish(RunEvent::Stopped).await.unwrap();
    let requests = server.requests().await;
    assert_eq!(server.endpoint_count("reply").await, 1);
    assert_eq!(
        post_text(requests.iter().find(|r| r.endpoint() == "reply").unwrap()),
        "正文".repeat(25)
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.contains("/complete/"))
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.method == "PUT" && r.path.contains("/messages/"))
    );
}

#[tokio::test]
async fn process_failure_does_not_suppress_the_final_answer() {
    let fail = Arc::new(AtomicBool::new(false));
    let server = HttpMockServer::start({
        let fail = fail.clone();
        move |request| {
            if request.method == "PUT" && fail.load(Ordering::SeqCst) {
                MockResponse::json(r#"{"code":99991672,"msg":"permission denied"}"#)
            } else {
                response(request)
            }
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    fail.store(true, Ordering::SeqCst);
    reply.publish(answer("完成的回答")).await.unwrap();
    assert!(
        reply
            .publish(RunEvent::Completed { exit_code: 0 })
            .await
            .is_err()
    );
    let requests = server.requests().await;
    assert_eq!(
        post_text(requests.iter().find(|r| r.endpoint() == "reply").unwrap()),
        "完成的回答"
    );
    assert!(!reply.inner.state.lock().await.terminal_retry_started);
}

#[tokio::test]
async fn answer_creation_retries_keep_uuid_and_original_content_even_when_answer_grows() {
    let attempts = AtomicUsize::new(0);
    let server = HttpMockServer::start(move |request| {
        if request.endpoint() == "reply" && attempts.fetch_add(1, Ordering::SeqCst) < 3 {
            MockResponse::json("{}").with_status(503)
        } else {
            response(request)
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    reply.publish(answer("前半段")).await.unwrap();
    assert!(reply.flush_answer().await.is_err());
    reply.publish(answer("后半段")).await.unwrap();
    reply
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    let requests = server.requests().await;
    let posts: Vec<_> = requests
        .iter()
        .filter(|r| r.endpoint() == "reply")
        .collect();
    assert_eq!(posts.len(), 4);
    assert!(posts.windows(2).all(|pair| pair[0].body == pair[1].body));
    let edits: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/open-apis/im/v1/messages/answer")
        .collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].method, "PUT");
    assert_eq!(post_text(edits[0]), "前半段后半段");
}

#[tokio::test]
async fn slow_network_does_not_hold_the_content_lock_and_new_process_events_are_not_lost() {
    let slow = Arc::new(AtomicBool::new(false));
    let server = HttpMockServer::start({
        let slow = slow.clone();
        move |request| {
            if request.method == "PUT" && slow.load(Ordering::SeqCst) {
                response(request).with_delay(Duration::from_millis(100))
            } else {
                response(request)
            }
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    slow.store(true, Ordering::SeqCst);
    reply.publish(commentary("first", "首段")).await.unwrap();
    let flush = tokio::spawn({
        let reply = reply.clone();
        async move { reply.flush_latest().await }
    });
    server.wait_for_method_count("PUT", 2).await;
    tokio::time::timeout(
        Duration::from_millis(50),
        reply.publish(commentary("second", "后段")),
    )
    .await
    .unwrap()
    .unwrap();
    flush.await.unwrap().unwrap();
    reply
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    let all = events(&server.requests().await);
    assert!(all.iter().any(|e| event_content(e)["delta"] == "首段"));
    assert!(all.iter().any(|e| event_content(e)["delta"] == "后段"));
    assert!(reply.inner.state.lock().await.content.pending.is_empty());
}

#[tokio::test]
async fn terminal_retry_finishes_in_the_run_and_does_not_duplicate_messages() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server = HttpMockServer::start({
        let attempts = attempts.clone();
        move |request| {
            if request.path.contains("/complete/") && attempts.fetch_add(1, Ordering::SeqCst) < 3 {
                MockResponse::json("{}").with_status(503)
            } else {
                response(request)
            }
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    reply.publish(answer("已完成")).await.unwrap();
    reply
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    drop(reply);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    assert_eq!(server.endpoint_count("reply").await, 1);
    assert_eq!(
        server
            .requests()
            .await
            .iter()
            .filter(|r| r.path.contains("message_cot?"))
            .count(),
        1
    );
}

#[tokio::test]
async fn terminal_arriving_during_publication_is_appended_before_native_completion() {
    let server = HttpMockServer::start(|request| {
        if request.method == "PUT" {
            response(request).with_delay(Duration::from_millis(50))
        } else {
            response(request)
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    reply.publish(commentary("c", "检查中")).await.unwrap();
    let flush = tokio::spawn({
        let reply = reply.clone();
        async move { reply.flush_latest().await }
    });
    server.wait_for_method_count("PUT", 2).await;
    reply.publish(RunEvent::Stopped).await.unwrap();
    flush.await.unwrap().unwrap();
    let requests = server.requests().await;
    let terminal = requests
        .iter()
        .position(|r| r.method == "PUT" && r.body.contains("RUN_FINISHED"))
        .unwrap();
    let complete = requests
        .iter()
        .position(|r| r.path.contains("/complete/"))
        .unwrap();
    assert!(terminal < complete);
    assert!(
        requests[complete]
            .path
            .ends_with("message_id=process&reason=error")
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.contains("/complete/"))
            .count(),
        1
    );
}

#[tokio::test]
async fn terminal_publication_waits_for_recovery_before_the_run_can_exit() {
    let attempts = AtomicUsize::new(0);
    let server = HttpMockServer::start(move |request| {
        if request.endpoint() == "reply" && attempts.fetch_add(1, Ordering::SeqCst) < 3 {
            MockResponse::json("{}").with_status(503)
        } else {
            response(request)
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    reply.publish(answer("最终回答")).await.unwrap();
    reply
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    assert_eq!(server.endpoint_count("reply").await, 4);
}
