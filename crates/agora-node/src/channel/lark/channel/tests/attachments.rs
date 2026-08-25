use super::*;
use crate::channel::test_http::{HttpMockServer, MockResponse};

#[tokio::test]
async fn downloads_a_lark_message_image_resource() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            let size = stream.read(&mut buffer).await.unwrap();
            if size == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..size]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with(
            "GET /open-apis/im/v1/messages/om_post_1/resources/img_trace?type=image HTTP/1.1"
        ));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer token")
        );

        let body = b"image-bytes";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    });
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        base_url,
    )
    .unwrap();

    let image = api
        .download_message_image("token", "om_post_1", "img_trace", usize::MAX)
        .await
        .unwrap();

    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.data, b"image-bytes");
    server.await.unwrap();
}

#[tokio::test]
async fn resolves_lark_post_images_into_task_attachments() {
    let LarkEvent::Message(event) = LarkEvent::from_lark_event_payload(
        r#"{
            "schema": "2.0",
            "header": {"event_id": "evt_post_1", "event_type": "im.message.receive_v1"},
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let size = stream.read(&mut buffer).await.unwrap();
                if size == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..size]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            let (content_type, body) = if request.contains("tenant_access_token/internal") {
                (
                    "application/json",
                    br#"{"code":0,"msg":"ok","tenant_access_token":"token"}"#.as_slice(),
                )
            } else {
                assert!(request.contains(
                    "/open-apis/im/v1/messages/om_post_1/resources/img_trace?type=image"
                ));
                ("image/png", b"image-bytes".as_slice())
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        }
    });
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        base_url,
    )
    .unwrap();
    let channel = LarkChannel::with_api(api);

    let task = channel.task_from_event(event).await.unwrap();

    let content = task.input().message().unwrap();
    assert_eq!(content.text(), "analyze this image");
    let [image] = content.attachments() else {
        panic!("task should contain one image");
    };
    assert_eq!(image.kind(), TaskAttachmentKind::Image);
    assert_eq!(image.file_name(), "lark-image-1.png");
    assert_eq!(image.media_type(), "image/png");
    assert_eq!(image.data(), b"image-bytes");
    server.await.unwrap();
}

#[tokio::test]
async fn rejects_lark_post_images_above_the_cumulative_attachment_limit() {
    let LarkEvent::Message(event) = LarkEvent::from_lark_event_payload(
        r#"{
            "schema": "2.0",
            "header": {"event_id": "evt_post_2", "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_123"}},
                "message": {
                    "message_id": "om_post_2",
                    "chat_id": "oc_123",
                    "chat_type": "group",
                    "message_type": "post",
                    "content": "{\"title\":\"\",\"content\":[[{\"tag\":\"img\",\"image_key\":\"first\"},{\"tag\":\"img\",\"image_key\":\"second\"}]]}"
                }
            }
        }"#,
    )
    .unwrap() else {
        panic!("receive event should contain a message");
    };
    let server = HttpMockServer::start(|request| {
        if request.path.contains("tenant_access_token/internal") {
            MockResponse::json(r#"{"code":0,"msg":"ok","tenant_access_token":"token"}"#)
        } else {
            MockResponse::bytes(b"123456".to_vec(), "image/png")
        }
    })
    .await;
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

    let error = channel
        .task_from_event_with_attachment_limit(event, 10)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("maximum 4 bytes"));
}

#[tokio::test]
async fn rejects_excessive_lark_image_counts_before_downloading() {
    let mut event = match LarkEvent::from_lark_event_payload(
        r#"{
            "schema": "2.0",
            "header": {"event_id": "evt_many_images", "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_123"}},
                "message": {
                    "message_id": "om_many_images",
                    "chat_id": "oc_123",
                    "chat_type": "group",
                    "message_type": "post",
                    "content": "{\"title\":\"\",\"content\":[[{\"tag\":\"text\",\"text\":\"images\"}]]}"
                }
            }
        }"#,
    )
    .unwrap()
    {
        LarkEvent::Message(event) => event,
        _ => panic!("receive event should contain a message"),
    };
    event.image_keys = (0..17).map(|index| format!("image-{index}")).collect();
    let api = LarkApi::with_base_url(
        LarkChannelConfig {
            name: "lark-test".to_string(),
            app_id: "app-id".to_string(),
            secret: "secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        "http://127.0.0.1:1".to_string(),
    )
    .unwrap();
    let channel = LarkChannel::with_api(api);

    let error = channel.task_from_event(event).await.unwrap_err();

    assert!(error.to_string().contains("at most 16 images"));
    assert!(
        error
            .downcast_ref::<LarkImageDownloadError>()
            .is_some_and(LarkImageDownloadError::is_permanent)
    );
}
