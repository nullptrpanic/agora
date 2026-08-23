use super::super::TelegramReplyTarget;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn telegram_api_uses_an_authenticated_http_proxy() {
    let proxy = HttpMockServer::start_json_queue([
        r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
    ])
    .await;
    let mut config = telegram_config();
    config.proxy = Some(
        format!(
            "user:password@{}",
            proxy.base_url().trim_start_matches("http://")
        )
        .parse()
        .unwrap(),
    );
    let api = TelegramApi::with_base_url(config, "http://telegram.invalid".to_string()).unwrap();

    assert_eq!(api.bot_username().await.unwrap(), "agora_bot");

    let requests = proxy.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].path,
        "http://telegram.invalid/bot123456:secret/getMe"
    );
    assert_eq!(
        requests[0].header("proxy-authorization"),
        Some("Basic dXNlcjpwYXNzd29yZA==")
    );
}

#[tokio::test]
async fn telegram_api_gets_identity_and_polls_message_updates() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        r#"{"ok":true,"result":[{"update_id":201,"message":{"message_id":9,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"hello"}}]}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    assert_eq!(api.bot_username().await.unwrap(), "agora_bot");
    let updates = api.get_updates(Some(42)).await.unwrap();

    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["update_id"], 201);
    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/bot123456:secret/getMe");
    assert_eq!(requests[1].path, "/bot123456:secret/getUpdates");
    let poll: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(poll["offset"], 42);
    assert_eq!(poll["timeout"], 50);
    assert_eq!(poll["limit"], 1);
    assert_eq!(
        poll["allowed_updates"],
        serde_json::json!(["message", "callback_query"])
    );
}

#[tokio::test]
async fn telegram_channel_waits_for_acceptance_before_advancing_offset() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        r#"{"ok":true,"result":true}"#,
        r#"{"ok":true,"result":[{"update_id":701,"message":{"message_id":31,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"first"}}]}"#,
        r#"{"ok":true,"result":[{"update_id":702,"message":{"message_id":32,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"second"}}]}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    let first = channel.recv().await.unwrap().unwrap();
    assert_eq!(first.input().message().unwrap().text(), "first");
    let (_, receipt) = first.into_parts();
    let mut second = Box::pin(channel.recv());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut second)
            .await
            .is_err()
    );

    receipt.accept();
    let second = tokio::time::timeout(std::time::Duration::from_secs(1), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second.input().message().unwrap().text(), "second");
    let requests = server.requests().await;
    let poll: serde_json::Value = serde_json::from_str(&requests[3].body).unwrap();
    assert_eq!(poll["offset"], 702);
}

#[tokio::test]
async fn telegram_channel_retries_a_delivery_without_advancing_offset() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        r#"{"ok":true,"result":true}"#,
        r#"{"ok":true,"result":[{"update_id":801,"message":{"message_id":41,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"retry me"}}]}"#,
        r#"{"ok":true,"result":[{"update_id":801,"message":{"message_id":41,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"retry me"}}]}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    let first = channel.recv().await.unwrap().unwrap();
    drop(first);
    let retried = channel.recv().await.unwrap().unwrap();

    assert_eq!(retried.task_id(), "801");
    let requests = server.requests().await;
    let retry_poll: serde_json::Value = serde_json::from_str(&requests[3].body).unwrap();
    assert_eq!(retry_poll.get("offset"), None);
}

#[tokio::test]
async fn telegram_api_registers_bot_commands() {
    let server = HttpMockServer::start_json_queue([r#"{"ok":true,"result":true}"#]).await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    api.set_commands(&[
        TelegramBotCommand::new("stop", "停止当前任务。"),
        TelegramBotCommand::new("help", "显示所有命令。"),
    ])
    .await
    .unwrap();

    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/bot123456:secret/setMyCommands");
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
        body["commands"],
        serde_json::json!([
            {"command": "stop", "description": "停止当前任务。"},
            {"command": "help", "description": "显示所有命令。"}
        ])
    );
}

#[tokio::test]
async fn telegram_api_rejects_false_command_registration_result() {
    let server = HttpMockServer::start_json_queue([r#"{"ok":true,"result":false}"#]).await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    let error = api
        .set_commands(&[TelegramBotCommand::new("help", "显示所有命令。")])
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "telegram setMyCommands returned false");
}

#[tokio::test]
async fn telegram_api_retries_after_rate_limit() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":0}}"#,
        r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    assert_eq!(api.bot_username().await.unwrap(), "agora_bot");
    assert_eq!(server.requests().await.len(), 2);
}

#[tokio::test]
async fn telegram_api_retries_server_errors() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":false,"error_code":502,"description":"Bad Gateway"}"#,
        r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    assert_eq!(api.bot_username().await.unwrap(), "agora_bot");
    assert_eq!(server.requests().await.len(), 3);
}

#[tokio::test]
async fn telegram_api_retries_invalid_server_error_responses() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = Arc::clone(&attempts);
    let server = HttpMockServer::start(move |_| {
        if captured.fetch_add(1, Ordering::SeqCst) < 2 {
            MockResponse::json("not-json").with_status(503)
        } else {
            MockResponse::json(
                r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
            )
        }
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    assert_eq!(api.bot_username().await.unwrap(), "agora_bot");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn telegram_api_retries_an_invalid_rate_limit_response() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = Arc::clone(&attempts);
    let server = HttpMockServer::start(move |_| {
        if captured.fetch_add(1, Ordering::SeqCst) == 0 {
            MockResponse::json("not-json").with_status(429)
        } else {
            MockResponse::json(
                r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
            )
        }
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    assert_eq!(api.bot_username().await.unwrap(), "agora_bot");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn telegram_api_retries_connection_failures_and_redacts_the_token() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let api = TelegramApi::with_base_url(telegram_config(), format!("http://{address}")).unwrap();

    let error = api.bot_username().await.unwrap_err().to_string();

    assert!(error.contains("connection failed"));
    assert!(!error.contains("123456:secret"));
}

#[tokio::test]
async fn telegram_api_rejects_false_callback_draft_and_failed_download_results() {
    let server = HttpMockServer::start(|request| match request.endpoint() {
        "answerCallbackQuery" | "sendRichMessageDraft" => {
            MockResponse::json(r#"{"ok":true,"result":false}"#)
        }
        "getFile" => MockResponse::json(
            r#"{"ok":true,"result":{"file_id":"file","file_unique_id":"unique","file_path":"files/image.jpg"}}"#,
        ),
        "image.jpg" => MockResponse::bytes(Vec::new(), "image/jpeg").with_status(503),
        endpoint => panic!("unexpected Telegram endpoint {endpoint}"),
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let target = TelegramReplyTarget {
        chat_id: 1,
        message_id: 2,
        message_thread_id: None,
        is_private: true,
    };

    assert!(api.answer_callback_query("query").await.is_err());
    assert!(
        api.send_rich_message_draft(&target, 7, "draft")
            .await
            .is_err()
    );
    assert!(api.download_file("file", usize::MAX).await.is_err());
}

#[tokio::test]
async fn telegram_image_download_rejects_an_oversized_body() {
    let server = HttpMockServer::start(|request| match request.endpoint() {
        "getFile" => MockResponse::json(
            r#"{"ok":true,"result":{"file_id":"file","file_unique_id":"unique","file_path":"files/image.jpg"}}"#,
        ),
        "image.jpg" => MockResponse::bytes(b"oversized".to_vec(), "image/jpeg"),
        endpoint => panic!("unexpected Telegram endpoint {endpoint}"),
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    let error = api
        .download_file("file", 4)
        .await
        .err()
        .expect("oversized image must be rejected");

    assert!(error.to_string().contains("maximum 4 bytes"));
}

#[tokio::test]
async fn telegram_api_does_not_retry_non_idempotent_message_sends() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":true,"result":{"message_id":88}}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let target = TelegramReplyTarget {
        chat_id: 1,
        message_id: 31,
        message_thread_id: None,
        is_private: true,
    };

    let error = api
        .send_rich_message(&target, "reply", None)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("sendRichMessage"));
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[tokio::test]
async fn telegram_api_errors_do_not_expose_the_bot_token() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    let error = api.bot_username().await.unwrap_err().to_string();

    assert!(error.contains("getMe"));
    assert!(error.contains("401"));
    assert!(error.contains("Unauthorized"));
    assert!(!error.contains("123456:secret"));
}

#[tokio::test]
async fn telegram_transport_errors_do_not_expose_the_bot_token() {
    let server = HttpMockServer::start_json_queue(["not-json"]).await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();

    let error = api.bot_username().await.unwrap_err();
    let report = format!("{error:#}");

    assert!(report.contains("getMe"));
    assert!(!report.contains("123456:secret"));
}

#[tokio::test]
async fn telegram_channel_returns_supported_updates_in_order_and_advances_offset() {
    crate::channel::test_http::enable_test_logging();
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        r#"{"ok":true,"result":true}"#,
        r#"{"ok":true,"result":[
            {"update_id":301,"message":{"message_id":21,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"first"}},
            {"update_id":302,"message":{"message_id":22,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"   "}},
            {"update_id":303,"message":{"message_id":23,"from":{"id":42,"is_bot":false},"message_thread_id":44,"chat":{"id":-1001,"type":"supergroup"},"text":"second"}}
        ]}"#,
        r#"{"ok":true,"result":[
            {"update_id":304,"message":{"message_id":24,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"third"}}
        ]}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    let first = channel.next_task().await.unwrap();
    let second = channel.next_task().await.unwrap();
    let third = channel.next_task().await.unwrap();

    assert_eq!(first.input().message().unwrap().text(), "first");
    assert_eq!(second.input().message().unwrap().text(), "second");
    assert_eq!(third.input().message().unwrap().text(), "third");
    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].path, "/bot123456:secret/setMyCommands");
    let commands: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(commands["commands"].as_array().unwrap().len(), 4);
    assert_eq!(commands["commands"][0]["command"], "stop");
    assert_eq!(commands["commands"][1]["command"], "reset");
    assert_eq!(commands["commands"][2]["command"], "ask");
    assert_eq!(commands["commands"][3]["command"], "help");
    let second_poll: serde_json::Value = serde_json::from_str(&requests[3].body).unwrap();
    assert_eq!(second_poll["offset"], 304);
}

#[tokio::test]
async fn telegram_channel_downloads_the_largest_photo_as_an_attachment() {
    use crate::task::TaskAttachmentKind;

    let server = HttpMockServer::start(|request| match (request.method.as_str(), request.endpoint()) {
        ("POST", "getMe") => MockResponse::json(
            r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        ),
        ("POST", "setMyCommands") => MockResponse::json(r#"{"ok":true,"result":true}"#),
        ("POST", "getUpdates") => MockResponse::json(
            r#"{"ok":true,"result":[{"update_id":305,"message":{"message_id":25,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"caption":"inspect","photo":[{"file_id":"small"},{"file_id":"large"}]}}]}"#,
        ),
        ("POST", "getFile") => MockResponse::json(
            r#"{"ok":true,"result":{"file_id":"large","file_unique_id":"unique","file_path":"photos/image.jpg"}}"#,
        ),
        ("GET", "image.jpg") => MockResponse::bytes(b"image-bytes".to_vec(), "image/jpeg"),
        (method, endpoint) => panic!("unexpected Telegram request {method} {endpoint}"),
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    let task = channel.next_task().await.unwrap();

    let content = task.input().message().unwrap();
    assert_eq!(content.text(), "inspect");
    let [image] = content.attachments() else {
        panic!("task should contain one image");
    };
    assert_eq!(image.kind(), TaskAttachmentKind::Image);
    assert_eq!(image.file_name(), "image.jpg");
    assert_eq!(image.media_type(), "image/jpeg");
    assert_eq!(image.data(), b"image-bytes");
    let requests = server.requests().await;
    let get_file = requests
        .iter()
        .find(|request| request.endpoint() == "getFile")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&get_file.body).unwrap()["file_id"],
        "large"
    );
    assert!(requests.iter().any(|request| {
        request.method == "GET" && request.path == "/file/bot123456:secret/photos/image.jpg"
    }));
}

#[tokio::test]
async fn telegram_channel_retries_an_image_update_without_advancing_its_offset() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let get_file_attempts = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&get_file_attempts);
    let server = HttpMockServer::start(move |request| match request.endpoint() {
        "getMe" => MockResponse::json(
            r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        ),
        "setMyCommands" => MockResponse::json(r#"{"ok":true,"result":true}"#),
        "getUpdates" => MockResponse::json(
            r#"{"ok":true,"result":[{"update_id":305,"message":{"message_id":25,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"caption":"inspect","photo":[{"file_id":"large"}]}}]}"#,
        ),
        "getFile" if attempts.fetch_add(1, Ordering::Relaxed) < 3 => MockResponse::json(
            r#"{"ok":false,"error_code":500,"description":"temporary failure"}"#,
        )
        .with_status(500),
        "getFile" => MockResponse::json(
            r#"{"ok":true,"result":{"file_id":"large","file_unique_id":"unique","file_path":"photos/image.jpg"}}"#,
        ),
        "image.jpg" => MockResponse::bytes(b"image-bytes".to_vec(), "image/jpeg"),
        endpoint => panic!("unexpected Telegram endpoint {endpoint}"),
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    assert!(channel.next_task().await.is_err());
    let task = channel.next_task().await.unwrap();

    assert_eq!(task.task_id(), "305");
    let polls = server
        .requests()
        .await
        .into_iter()
        .filter(|request| request.endpoint() == "getUpdates")
        .collect::<Vec<_>>();
    assert_eq!(polls.len(), 2);
    let retry: serde_json::Value = serde_json::from_str(&polls[1].body).unwrap();
    assert_eq!(retry.get("offset"), None);
}

#[tokio::test]
async fn telegram_channel_skips_a_permanently_invalid_image_update() {
    let server = HttpMockServer::start(|request| match request.endpoint() {
        "getMe" => MockResponse::json(
            r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        ),
        "setMyCommands" => MockResponse::json(r#"{"ok":true,"result":true}"#),
        "getUpdates" => MockResponse::json(
            r#"{"ok":true,"result":[
                {"update_id":305,"message":{"message_id":25,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"photo":[{"file_id":"invalid"}]}},
                {"update_id":306,"message":{"message_id":26,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"after invalid image"}}
            ]}"#,
        ),
        "getFile" => MockResponse::json(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: wrong file identifier"}"#,
        )
        .with_status(400),
        endpoint => panic!("unexpected Telegram endpoint {endpoint}"),
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    let task = channel.next_task().await.unwrap();

    assert_eq!(task.task_id(), "306");
    assert_eq!(
        task.input().message().unwrap().text(),
        "after invalid image"
    );
    assert_eq!(server.endpoint_count("getFile").await, 1);
    assert_eq!(server.endpoint_count("getUpdates").await, 1);
}

#[tokio::test]
async fn telegram_channel_stops_retrying_a_transient_image_failure() {
    let server = HttpMockServer::start(|request| match request.endpoint() {
        "getMe" => MockResponse::json(
            r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        ),
        "setMyCommands" => MockResponse::json(r#"{"ok":true,"result":true}"#),
        "getUpdates" => MockResponse::json(
            r#"{"ok":true,"result":[
                {"update_id":305,"message":{"message_id":25,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"photo":[{"file_id":"temporary"}]}},
                {"update_id":306,"message":{"message_id":26,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"after retries"}}
            ]}"#,
        ),
        "getFile" => MockResponse::json(
            r#"{"ok":true,"result":{"file_id":"temporary","file_unique_id":"unique","file_path":"photos/image.jpg"}}"#,
        ),
        "image.jpg" => MockResponse::bytes(Vec::new(), "image/jpeg").with_status(503),
        endpoint => panic!("unexpected Telegram endpoint {endpoint}"),
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    assert!(channel.next_task().await.is_err());
    assert!(channel.next_task().await.is_err());
    let task = channel.next_task().await.unwrap();

    assert_eq!(task.task_id(), "306");
    assert_eq!(task.input().message().unwrap().text(), "after retries");
    assert_eq!(server.endpoint_count("getFile").await, 3);
    assert_eq!(server.endpoint_count("image.jpg").await, 3);
    assert_eq!(server.endpoint_count("getUpdates").await, 3);
}

#[tokio::test]
async fn telegram_channel_skips_an_oversized_image_without_retrying() {
    let server = HttpMockServer::start(|request| match request.endpoint() {
        "getMe" => MockResponse::json(
            r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}}"#,
        ),
        "setMyCommands" => MockResponse::json(r#"{"ok":true,"result":true}"#),
        "getUpdates" => MockResponse::json(
            r#"{"ok":true,"result":[
                {"update_id":305,"message":{"message_id":25,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"photo":[{"file_id":"oversized"}]}},
                {"update_id":306,"message":{"message_id":26,"from":{"id":42,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"after oversized image"}}
            ]}"#,
        ),
        "getFile" => MockResponse::json(
            r#"{"ok":true,"result":{"file_id":"oversized","file_unique_id":"unique","file_path":"photos/image.jpg"}}"#,
        ),
        "image.jpg" => MockResponse::bytes(Vec::new(), "image/jpeg")
            .with_declared_content_length(crate::http::MAX_TASK_ATTACHMENT_BYTES + 1),
        endpoint => panic!("unexpected Telegram endpoint {endpoint}"),
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api(api);

    let task = channel.next_task().await.unwrap();

    assert_eq!(task.task_id(), "306");
    assert_eq!(server.endpoint_count("getFile").await, 1);
    assert_eq!(server.endpoint_count("image.jpg").await, 1);
    assert_eq!(server.endpoint_count("getUpdates").await, 1);
}

#[tokio::test]
async fn telegram_channel_reports_command_reply_delivery_failures() {
    let server = HttpMockServer::start_json_queue([
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
        r#"{"ok":true,"result":{"message_id":88}}"#,
    ])
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let channel = ConfiguredChannel::Telegram(TelegramChannel::with_api(api));
    let task = ConfiguredTask::Telegram(
        TelegramUpdate::from_json(
            r#"{
            "update_id": 401,
            "message": {
                "message_id": 31,
                "from": {"id": 42, "is_bot": false},
                "chat": {"id": 1, "type": "private"},
                "text": "/help"
            }
        }"#,
        )
        .unwrap()
        .into_task("agora_bot")
        .unwrap(),
    );

    let error = channel
        .reply(&task, ChannelReply::new("**Agora 命令**"))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("sendRichMessage"));
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
}

#[tokio::test]
async fn telegram_channel_replies_to_denied_messages_and_returns_the_next_allowed_task() {
    let server = HttpMockServer::start(|request| {
        let result = match request.endpoint() {
            "getMe" => r#"{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}"#,
            "setMyCommands" => "true",
            "getUpdates" => {
                r#"[
                    {
                        "update_id":601,
                        "message":{
                            "message_id":41,
                            "from":{"id":7,"is_bot":false},
                            "chat":{"id":-1001,"type":"group"},
                            "text":"denied @agora_bot"
                        }
                    },
                    {
                        "update_id":602,
                        "message":{
                            "message_id":42,
                            "from":{"id":42,"is_bot":false},
                            "chat":{"id":-1001,"type":"group"},
                            "text":"allowed"
                        }
                    }
                ]"#
            }
            "sendRichMessage" => r#"{"message_id":88}"#,
            method => panic!("unexpected Telegram method {method}"),
        };
        MockResponse::json(format!(r#"{{"ok":true,"result":{result}}}"#))
    })
    .await;
    let access = permission(&["42"], &[("-1001", false)]);
    let mut config = telegram_config();
    config.permission = access.clone();
    let api = TelegramApi::with_base_url(config, server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api_and_permission(api, access);

    let task = channel.next_task().await.unwrap();

    assert_eq!(task.input().message().unwrap().text(), "allowed");
    let requests = server.requests().await;
    let denial = requests
        .iter()
        .find(|request| request.endpoint() == "sendRichMessage")
        .unwrap();
    assert!(denial.body.contains("Channel：`telegram-test`"));
    assert!(denial.body.contains("User ID：`7`"));
    assert!(denial.body.contains("Group ID：`-1001`"));
}

#[tokio::test]
async fn telegram_group_mention_requirement_matches_only_the_current_bot() {
    let server = HttpMockServer::start(|request| {
        let result = match request.endpoint() {
            "getMe" => r#"{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}"#,
            "setMyCommands" => "true",
            "getUpdates" => {
                r#"[
                    {
                        "update_id":611,
                        "message":{
                            "message_id":51,
                            "from":{"id":42,"is_bot":false},
                            "chat":{"id":-1001,"type":"group"},
                            "text":"hello @other_bot"
                        }
                    },
                    {
                        "update_id":612,
                        "message":{
                            "message_id":52,
                            "from":{"id":42,"is_bot":false},
                            "chat":{"id":-1001,"type":"group"},
                            "text":"hello @Agora_Bot"
                        }
                    }
                ]"#
            }
            "sendRichMessage" => r#"{"message_id":89}"#,
            method => panic!("unexpected Telegram method {method}"),
        };
        MockResponse::json(format!(r#"{{"ok":true,"result":{result}}}"#))
    })
    .await;
    let access = permission(&["42"], &[("-1001", true)]);
    let mut config = telegram_config();
    config.permission = access.clone();
    let api = TelegramApi::with_base_url(config, server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api_and_permission(api, access);

    let task = channel.next_task().await.unwrap();

    assert_eq!(task.reply_target().message_id, 52);
    let requests = server.requests().await;
    assert!(
        !requests
            .iter()
            .any(|request| request.endpoint() == "sendRichMessage")
    );
}

#[tokio::test]
async fn telegram_run_button_interrupts_the_run_and_is_removed_after_stop() {
    let callback_data = Arc::new(Mutex::new(None::<String>));
    let response_callback_data = Arc::clone(&callback_data);
    let server = HttpMockServer::start(move |request| {
        let result = match request.endpoint() {
            "getMe" => r#"{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}"#,
            "setMyCommands" | "answerCallbackQuery" => "true",
            "sendRichMessage" | "editMessageText" => r#"{"message_id":88}"#,
            "getUpdates" => {
                let callback_data = response_callback_data
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("active run must publish callback data before polling");
                return MockResponse::json(
                    serde_json::json!({
                        "ok": true,
                        "result": [
                            {
                                "update_id": 501,
                                "callback_query": {
                                    "id": "callback-1",
                                    "from": {"id": 42, "is_bot": false},
                                    "message": {
                                        "message_id": 88,
                                        "chat": {"id": 1, "type": "private"}
                                    },
                                    "data": callback_data
                                }
                            },
                            {
                                "update_id": 502,
                                "message": {
                                    "message_id": 32,
                                    "from": {"id": 42, "is_bot": false},
                                    "chat": {"id": 1, "type": "private"},
                                    "text": "after stop"
                                }
                            }
                        ]
                    })
                    .to_string(),
                );
            }
            method => panic!("unexpected Telegram method {method}"),
        };
        MockResponse::json(format!(r#"{{"ok":true,"result":{result}}}"#))
    })
    .await;
    let api = TelegramApi::with_base_url(telegram_config(), server.base_url()).unwrap();
    let mut channel = ConfiguredChannel::Telegram(TelegramChannel::with_api(api));
    let source = ConfiguredTask::Telegram(
        TelegramUpdate::from_json(
            r#"{
            "update_id": 500,
            "message": {
                "message_id": 31,
                "from": {"id": 42, "is_bot": false},
                "chat": {"id": 1, "type": "private"},
                "text": "run"
            }
        }"#,
        )
        .unwrap()
        .into_task("agora_bot")
        .unwrap(),
    );
    let interrupted = Arc::new(AtomicBool::new(false));
    let callback_interrupted = Arc::clone(&interrupted);
    let run = channel
        .open_run(
            &source,
            ChannelRunContext {
                agent: ChannelAgent {
                    name: "codex".to_string(),
                },
                interrupt: Some(InterruptCallback::new(move || {
                    callback_interrupted.store(true, Ordering::Relaxed);
                    true
                })),
            },
        )
        .await
        .unwrap();

    run.publish(RunEvent::Started {
        run_id: "run-1".to_string(),
    })
    .await
    .unwrap();
    server.wait_for_endpoint_count("sendRichMessage", 1).await;
    let active = server
        .requests()
        .await
        .into_iter()
        .find(|request| request.endpoint() == "sendRichMessage")
        .unwrap();
    let active: serde_json::Value = serde_json::from_str(&active.body).unwrap();
    assert_eq!(
        active["rich_message"]["markdown"],
        format!(
            "## codex · ● 运行中\n\n> {}",
            crate::i18n::WAITING_FOR_AGENT
        )
    );
    assert!(
        !active["rich_message"]["markdown"]
            .as_str()
            .unwrap()
            .contains("<tg-thinking>")
    );
    assert_eq!(
        active["reply_markup"]["inline_keyboard"][0][0]["text"],
        "结束任务"
    );
    let active_callback_data = active["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(active_callback_data.starts_with("agora_interrupt:interrupt-"));
    *callback_data.lock().unwrap() = Some(active_callback_data);
    assert_eq!(
        active["reply_markup"]["inline_keyboard"][0][0]["style"],
        "danger"
    );

    run.publish(RunEvent::Output(crate::task::OutputEvent::Thinking {
        text: "Inspecting the project".to_string(),
    }))
    .await
    .unwrap();
    run.publish(RunEvent::Output(crate::task::OutputEvent::Answer {
        text: "Partial answer".to_string(),
    }))
    .await
    .unwrap();
    server.wait_for_endpoint_count("editMessageText", 1).await;
    assert_eq!(server.endpoint_count("sendRichMessage").await, 1);
    assert_eq!(server.endpoint_count("sendRichMessageDraft").await, 0);
    let streaming = server
        .requests()
        .await
        .into_iter()
        .find(|request| request.endpoint() == "editMessageText")
        .unwrap();
    let streaming: serde_json::Value = serde_json::from_str(&streaming.body).unwrap();
    assert_eq!(streaming["message_id"], 88);
    assert!(
        streaming["rich_message"]["markdown"]
            .as_str()
            .unwrap()
            .contains("Inspecting the project")
    );
    assert!(
        streaming["rich_message"]["markdown"]
            .as_str()
            .unwrap()
            .contains("Partial answer")
    );

    let next = channel.recv().await.unwrap().unwrap();
    assert_eq!(next.input().message().unwrap().text(), "after stop");
    assert!(interrupted.load(Ordering::Relaxed));
    server
        .wait_for_endpoint_count("answerCallbackQuery", 1)
        .await;
    run.publish(RunEvent::Stopped).await.unwrap();
    server.wait_for_endpoint_count("editMessageText", 2).await;
    let terminal = server
        .requests()
        .await
        .into_iter()
        .rfind(|request| request.endpoint() == "editMessageText")
        .unwrap();
    let terminal: serde_json::Value = serde_json::from_str(&terminal.body).unwrap();
    assert_eq!(
        terminal["reply_markup"]["inline_keyboard"],
        serde_json::json!([])
    );
}

#[test]
fn telegram_interrupt_callback_ids_do_not_repeat_after_restart() {
    let first = TelegramInterruptCallbacks::default()
        .register(InterruptCallback::new(|| true))
        .callback_data();
    let second = TelegramInterruptCallbacks::default()
        .register(InterruptCallback::new(|| true))
        .callback_data();

    assert_ne!(first, second);
}

#[tokio::test]
async fn telegram_callbacks_check_the_actor_without_requiring_a_new_mention() {
    let callback_data = Arc::new(Mutex::new(None::<String>));
    let response_callback_data = Arc::clone(&callback_data);
    let server = HttpMockServer::start(move |request| {
        let result = match request.endpoint() {
            "getMe" => r#"{"id":123,"is_bot":true,"first_name":"Agora","username":"agora_bot"}"#,
            "setMyCommands" | "answerCallbackQuery" => "true",
            "getUpdates" => {
                let callback_data = response_callback_data
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("interrupt must be registered before polling");
                return MockResponse::json(
                    serde_json::json!({
                        "ok": true,
                        "result": [
                            {
                                "update_id": 621,
                                "callback_query": {
                                    "id": "callback-denied",
                                    "from": {"id": 7, "is_bot": false},
                                    "message": {
                                        "message_id": 91,
                                        "message_thread_id": 44,
                                        "chat": {"id": -1001, "type": "group"}
                                    },
                                    "data": callback_data
                                }
                            },
                            {
                                "update_id": 622,
                                "message": {
                                    "message_id": 92,
                                    "from": {"id": 42, "is_bot": false},
                                    "chat": {"id": -1001, "type": "group"},
                                    "text": "@agora_bot continue"
                                }
                            }
                        ]
                    })
                    .to_string(),
                );
            }
            "sendRichMessage" => r#"{"message_id":93}"#,
            method => panic!("unexpected Telegram method {method}"),
        };
        MockResponse::json(format!(r#"{{"ok":true,"result":{result}}}"#))
    })
    .await;
    let access = permission(&["42"], &[("-1001", true)]);
    let mut config = telegram_config();
    config.permission = access.clone();
    let api = TelegramApi::with_base_url(config, server.base_url()).unwrap();
    let mut channel = TelegramChannel::with_api_and_permission(api, access);
    let interrupted = Arc::new(AtomicBool::new(false));
    let callback_interrupted = Arc::clone(&interrupted);
    let registration = channel.interrupts.register(InterruptCallback::new(move || {
        callback_interrupted.store(true, Ordering::Relaxed);
        true
    }));
    *callback_data.lock().unwrap() = Some(registration.callback_data());

    let task = channel.next_task().await.unwrap();

    assert_eq!(task.reply_target().message_id, 92);
    assert!(!interrupted.load(Ordering::Relaxed));
    server
        .wait_for_endpoint_count("answerCallbackQuery", 1)
        .await;
    let requests = server.requests().await;
    assert!(requests.iter().any(|request| {
        request.endpoint() == "sendRichMessage" && request.body.contains("User ID：`7`")
    }));
    assert!(
        requests
            .iter()
            .any(|request| request.endpoint() == "answerCallbackQuery")
    );
}
