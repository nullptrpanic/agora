use super::*;
use crate::task::OutputImage;

fn image_event() -> RunEvent {
    RunEvent::Output(OutputEvent::Image {
        id: "image-1".into(),
        image: OutputImage::new(b"\x89PNG\r\n\x1a\nfixture".to_vec()).unwrap(),
    })
}

fn card(server: &HttpMockServer) -> LarkAgentReply {
    LarkAgentReply::new(
        LarkReplyTarget {
            message_id: "source".into(),
            mention_user_id: Some("ou_sender".into()),
        },
        "chat".into(),
        "agent".into(),
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
    )
}

#[tokio::test]
async fn generated_image_uploads_once_and_quotes_the_source_without_forcing_a_thread() {
    let server = HttpMockServer::start(|request| {
        MockResponse::json(match request.endpoint() {
            "internal" => r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#,
            "images" => r#"{"code":0,"msg":"ok","data":{"image_key":"img_generated"}}"#,
            "reply" => r#"{"code":0,"msg":"ok","data":{"message_id":"reply","cot_id":"cot"}}"#,
            _ => r#"{"code":0,"msg":"ok","data":{"message_id":"process","cot_id":"cot"}}"#,
        })
    })
    .await;
    let card = card(&server);
    card.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    card.publish(image_event()).await.unwrap();
    card.publish(image_event()).await.unwrap();
    card.publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    card.publish(image_event()).await.unwrap();
    let requests = server.requests().await;
    let uploads = requests
        .iter()
        .filter(|request| request.endpoint() == "images")
        .collect::<Vec<_>>();
    assert_eq!(uploads.len(), 1);
    assert!(
        uploads[0]
            .header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary=")
    );
    assert!(
        uploads[0]
            .body
            .contains("name=\"image_type\"\r\n\r\nmessage")
    );
    assert!(uploads[0].body.contains("filename=\"image.png\""));
    assert!(uploads[0].body.contains("fixture"));
    assert!(
        uploads[0]
            .body_bytes
            .windows(15)
            .any(|bytes| bytes == b"\x89PNG\r\n\x1a\nfixture")
    );
    let images = requests
        .iter()
        .filter_map(|request| serde_json::from_str::<serde_json::Value>(&request.body).ok())
        .filter(|body| body["msg_type"] == "image")
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["reply_in_thread"], false);
    assert_eq!(images[0]["content"], r#"{"image_key":"img_generated"}"#);
    assert!(requests.iter().any(|request| request.path
        == "/open-apis/im/v1/messages/source/reply"
        && request.body.contains("img_generated")));
}

#[tokio::test]
async fn generated_image_failure_is_visible_and_does_not_fail_the_text_answer() {
    let server = HttpMockServer::start(|request| {
        if request.endpoint() == "internal" {
            MockResponse::json(
                r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#,
            )
        } else if request.endpoint() == "images" {
            MockResponse::json("{}").with_status(503)
        } else {
            MockResponse::json(
                r#"{"code":0,"msg":"ok","data":{"message_id":"reply","cot_id":"cot"}}"#,
            )
        }
    })
    .await;
    let card = card(&server);
    card.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    card.publish(image_event()).await.unwrap();
    card.publish(RunEvent::Output(OutputEvent::Answer {
        text: "正文保留".into(),
    }))
    .await
    .unwrap();
    card.publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    let requests = server.requests().await;
    let final_card = &requests.last().unwrap().body;
    assert!(final_card.contains("正文保留"));
    assert!(requests.iter().any(|r| r.body.contains("图片未确认送达")));
    assert!(requests.iter().any(|r| r.body.contains("RUN_FINISHED")));
}

#[tokio::test]
async fn generated_image_refreshes_rejected_auth_without_reuploading_an_existing_key() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for reject_upload in [true, false] {
        let tokens = AtomicUsize::new(0);
        let server = HttpMockServer::start(move |request| {
            if request.endpoint() == "internal" {
                let token = tokens.fetch_add(1, Ordering::SeqCst) + 1;
                return MockResponse::json(format!(
                    r#"{{"code":0,"msg":"ok","tenant_access_token":"token{token}","expire":7200}}"#
                ));
            }
            let image_reply = request.endpoint() == "reply" && request.body.contains("image_key");
            if request.header("authorization") == Some("Bearer token1")
                && ((reject_upload && request.endpoint() == "images")
                    || (!reject_upload && image_reply))
            {
                return MockResponse::json(r#"{"code":99991663,"msg":"token expired"}"#);
            }
            MockResponse::json(if request.endpoint() == "images" {
                r#"{"code":0,"msg":"ok","data":{"image_key":"image"}}"#
            } else {
                r#"{"code":0,"msg":"ok","data":{"message_id":"reply","cot_id":"cot"}}"#
            })
        })
        .await;
        let card = card(&server);
        card.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
        card.publish(image_event()).await.unwrap();
        card.publish(RunEvent::Completed { exit_code: 0 })
            .await
            .unwrap();
        assert_eq!(server.endpoint_count("internal").await, 2);
        assert_eq!(
            server.endpoint_count("images").await,
            if reject_upload { 2 } else { 1 }
        );
        assert!(
            server
                .requests()
                .await
                .iter()
                .any(|request| request.body.contains("图片附件已发送"))
        );
    }
}

#[tokio::test]
async fn slow_image_upload_does_not_lock_stop_publication_and_can_be_cancelled() {
    use std::time::Duration;
    let server = HttpMockServer::start(|request| {
        if request.endpoint() == "internal" {
            MockResponse::json(
                r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#,
            )
        } else if request.endpoint() == "images" {
            MockResponse::json(r#"{"code":0,"msg":"ok","data":{"image_key":"image"}}"#)
                .with_delay(Duration::from_secs(30))
        } else {
            MockResponse::json(
                r#"{"code":0,"msg":"ok","data":{"message_id":"reply","cot_id":"cot"}}"#,
            )
        }
    })
    .await;
    let card = card(&server);
    card.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    let upload = tokio::spawn({
        let card = card.clone();
        async move { card.publish(image_event()).await }
    });
    server.wait_for_endpoint_count("images", 1).await;
    tokio::time::timeout(Duration::from_secs(1), card.publish(RunEvent::Stopped))
        .await
        .unwrap()
        .unwrap();
    upload.abort();
    assert!(upload.await.unwrap_err().is_cancelled());
    card.publish(image_event()).await.unwrap();
    let requests = server.requests().await;
    assert_eq!(server.endpoint_count("images").await, 1);
    assert!(
        requests
            .iter()
            .any(|request| request.body.contains("已停止"))
    );
    assert_eq!(server.endpoint_count("reply").await, 0);
    assert!(
        !requests
            .iter()
            .any(|request| request.body.contains("image_key"))
    );
}

#[tokio::test]
async fn available_answer_precedes_image_upload_and_only_receives_one_terminal_edit() {
    let server = HttpMockServer::start(|request| {
        if request.endpoint() == "images" {
            MockResponse::json(r#"{"code":0,"msg":"ok","data":{"image_key":"image"}}"#)
        } else {
            response(request)
        }
    })
    .await;
    let reply = reply(&server);
    reply.publish(RunEvent::Queued { ahead: 0 }).await.unwrap();
    reply.publish(answer("图片说明")).await.unwrap();
    reply.publish(image_event()).await.unwrap();
    for _ in 0..25 {
        reply.publish(answer("后续文字")).await.unwrap();
        reply.flush_latest().await.unwrap();
    }
    reply
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    let requests = server.requests().await;
    let answer_index = requests
        .iter()
        .position(|r| r.endpoint() == "reply" && body(r)["msg_type"] == "post")
        .unwrap();
    let image_index = requests
        .iter()
        .position(|r| r.endpoint() == "images")
        .unwrap();
    assert!(answer_index < image_index);
    assert_eq!(post_text(&requests[answer_index]), "图片说明");
    let edits: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/open-apis/im/v1/messages/answer")
        .collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(
        post_text(edits[0]),
        format!("图片说明{}", "后续文字".repeat(25))
    );
}
