use super::*;
use crate::task::OutputImage;

fn image_event() -> RunEvent {
    RunEvent::Output(OutputEvent::Image {
        id: "image-1".into(),
        image: OutputImage::new(b"\x89PNG\r\n\x1a\nfixture".to_vec()).unwrap(),
    })
}

#[tokio::test]
async fn generated_image_is_sent_once_to_the_same_chat_and_topic() {
    let server =
        HttpMockServer::start(|_| MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#))
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
    message.publish(image_event()).await.unwrap();
    message.publish(image_event()).await.unwrap();
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    message.publish(image_event()).await.unwrap();
    let requests = server.requests().await;
    let photos = requests
        .iter()
        .filter(|request| request.endpoint() == "sendPhoto")
        .collect::<Vec<_>>();
    assert_eq!(photos.len(), 1);
    assert!(
        photos[0]
            .body_bytes
            .windows(15)
            .any(|bytes| bytes == b"\x89PNG\r\n\x1a\nfixture")
    );
    assert!(
        photos[0]
            .header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary=")
    );
    for field in [
        "name=\"chat_id\"\r\n\r\n-1001",
        "name=\"message_thread_id\"\r\n\r\n44",
        "{\"message_id\":12}",
        "filename=\"image.png\"",
        "fixture",
    ] {
        assert!(
            photos[0].body.contains(field),
            "missing multipart value {field}"
        );
    }
}

#[tokio::test]
async fn generated_image_send_failure_is_not_retried_as_a_new_photo() {
    let server = HttpMockServer::start(|request| {
        if request.endpoint() == "sendPhoto" {
            MockResponse::json(r#"{"ok":false,"error_code":503,"description":"unavailable"}"#)
                .with_status(503)
        } else {
            MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#)
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
    message.publish(image_event()).await.unwrap();
    message.publish(image_event()).await.unwrap();
    message
        .publish(RunEvent::Output(OutputEvent::Answer {
            text: "正文保留".into(),
        }))
        .await
        .unwrap();
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    assert_eq!(server.endpoint_count("sendPhoto").await, 1);
    let requests = server.requests().await;
    let final_message = &requests.last().unwrap().body;
    assert!(final_message.contains("图片未确认送达"));
    assert!(final_message.contains("正文保留"));
}

#[tokio::test]
async fn private_image_reply_uses_source_message_without_a_forum_topic() {
    let server = HttpMockServer::start(|request| {
        MockResponse::json(if request.endpoint() == "sendRichMessageDraft" {
            r#"{"ok":true,"result":true}"#
        } else {
            r#"{"ok":true,"result":{"message_id":100}}"#
        })
    })
    .await;
    let mut target = private_target();
    target.message_thread_id = None;
    let message = TelegramRichMessage::with_timing(
        target,
        "agent".into(),
        telegram_api(&server),
        TelegramRichTiming::new(Duration::ZERO, Duration::from_secs(3600)),
    );
    message
        .publish(RunEvent::Queued { ahead: 0 })
        .await
        .unwrap();
    message.publish(image_event()).await.unwrap();
    message
        .publish(RunEvent::Completed { exit_code: 0 })
        .await
        .unwrap();
    let requests = server.requests().await;
    let photo = requests
        .iter()
        .find(|request| request.endpoint() == "sendPhoto")
        .unwrap();
    assert!(photo.body.contains("name=\"chat_id\"\r\n\r\n1"));
    assert!(photo.body.contains("{\"message_id\":7}"));
    assert!(!photo.body.contains("message_thread_id"));
}

#[tokio::test]
async fn slow_photo_send_does_not_lock_stop_publication_and_can_be_cancelled() {
    let server = HttpMockServer::start(|request| {
        let response = MockResponse::json(r#"{"ok":true,"result":{"message_id":100}}"#);
        if request.endpoint() == "sendPhoto" {
            response.with_delay(Duration::from_secs(30))
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
    let upload = tokio::spawn({
        let message = message.clone();
        async move { message.publish(image_event()).await }
    });
    server.wait_for_endpoint_count("sendPhoto", 1).await;
    tokio::time::timeout(Duration::from_secs(1), message.publish(RunEvent::Stopped))
        .await
        .unwrap()
        .unwrap();
    upload.abort();
    assert!(upload.await.unwrap_err().is_cancelled());
    message.publish(image_event()).await.unwrap();
    assert_eq!(server.endpoint_count("sendPhoto").await, 1);
    assert!(
        server
            .requests()
            .await
            .last()
            .unwrap()
            .body
            .contains("已停止")
    );
}

#[tokio::test]
async fn malformed_photo_responses_and_transport_errors_do_not_expose_token() {
    for body in [
        "invalid json",
        r#"{"ok":true}"#,
        r#"{"ok":false,"error_code":400,"description":"123456:secret"}"#,
    ] {
        let server = HttpMockServer::start(move |_| MockResponse::json(body)).await;
        let RunEvent::Output(OutputEvent::Image { image, .. }) = image_event() else {
            unreachable!()
        };
        let error = telegram_api(&server)
            .send_photo(&group_target(), &image)
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains("123456:secret"));
        assert_eq!(server.endpoint_count("sendPhoto").await, 1);
    }
}
