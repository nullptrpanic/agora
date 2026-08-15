use super::super::channel::TelegramReplyTarget;
use super::super::telegram_api::TelegramApi;
use super::{TelegramRichContent, TelegramRichMessage, TelegramRichTiming};
use crate::channel::test_http::{HttpMockServer, MockResponse};
use crate::channel::{ChannelRun, RunEvent};
use crate::config::TelegramChannelConfig;
use crate::i18n;
use crate::task::{OutputEvent, ProgressStatus, TokenUsage};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

mod api;
mod content;

fn telegram_api(server: &HttpMockServer) -> TelegramApi {
    TelegramApi::with_base_url(
        TelegramChannelConfig {
            name: "telegram-test".to_string(),
            token: "123456:secret".to_string(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap()
}

fn private_target() -> TelegramReplyTarget {
    TelegramReplyTarget {
        chat_id: 1,
        message_id: 7,
        message_thread_id: Some(44),
        is_private: true,
    }
}

fn group_target() -> TelegramReplyTarget {
    TelegramReplyTarget {
        chat_id: -1001,
        message_id: 12,
        message_thread_id: Some(44),
        is_private: false,
    }
}

async fn rich_message_server() -> HttpMockServer {
    let next_message_id = AtomicI64::new(100);
    HttpMockServer::start(move |request| {
        let result = match request.endpoint() {
            "sendRichMessageDraft" => "true".to_string(),
            "sendRichMessage" => {
                let message_id = next_message_id.fetch_add(1, Ordering::Relaxed);
                format!(r#"{{"message_id":{message_id}}}"#)
            }
            "editMessageText" => r#"{"message_id":100}"#.to_string(),
            method => panic!("unexpected Telegram method {method}"),
        };
        MockResponse::json(format!(r#"{{"ok":true,"result":{result}}}"#))
    })
    .await
}
