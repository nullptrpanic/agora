use super::*;

#[test]
fn image_delivery_claims_are_deduplicated_and_bounded() {
    let image = OutputImage::new(b"\x89PNG\r\n\x1a\nfixture".to_vec()).unwrap();
    let mut deliveries = ImageDeliveries::default();
    for index in 0..MAX_OUTPUT_IMAGES {
        assert!(deliveries.admit(&index.to_string(), &image).unwrap());
        assert!(!deliveries.admit(&index.to_string(), &image).unwrap());
    }
    assert!(deliveries.admit("excess", &image).is_err());
    let mut bytes = vec![0; crate::task::MAX_OUTPUT_IMAGE_BYTES];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let image = OutputImage::new(bytes).unwrap();
    let mut deliveries = ImageDeliveries::default();
    for index in 0..3 {
        assert!(deliveries.admit(&index.to_string(), &image).unwrap());
    }
    assert!(deliveries.admit("too-large", &image).is_err());
    assert_eq!(deliveries.ids.len(), 3);
}
use crate::config::{LarkChannelConfig, TelegramChannelConfig};

#[tokio::test]
async fn channel_delivery_accepts_the_source_exactly_once() {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let (sender, received) = tokio::sync::oneshot::channel();
    let delivery = ChannelDelivery::new("task", deadline, move |disposition| {
        sender.send(disposition).unwrap();
    });

    assert_eq!(delivery.task(), &"task");
    let (task, receipt) = delivery.into_parts();
    assert_eq!(task, "task");
    assert_eq!(receipt.deadline(), deadline);
    receipt.accept();

    assert_eq!(received.await.unwrap(), DeliveryDisposition::Accepted);
}

#[tokio::test]
async fn unresolved_channel_delivery_requests_retry_after_mapping() {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let (sender, received) = tokio::sync::oneshot::channel();
    let delivery = ChannelDelivery::new(7_u32, deadline, move |disposition| {
        sender.send(disposition).unwrap();
    })
    .map(|task| task.to_string());

    assert_eq!(delivery.task(), "7");
    let (_, receipt) = delivery.into_parts();
    assert_eq!(receipt.deadline(), deadline);
    drop(receipt);

    assert_eq!(received.await.unwrap(), DeliveryDisposition::Retry);
}

#[test]
fn channel_value_objects_expose_buttons_statuses_replies_and_callbacks() {
    let command = CommandRequest::new(["agent", "enable"]).with_argument("agent_name", "reviewer");
    for style in [
        ChannelButtonStyle::Default,
        ChannelButtonStyle::Primary,
        ChannelButtonStyle::Danger,
    ] {
        let button = ChannelButton::new("Enable", style, command.clone());
        assert_eq!(button.text(), "Enable");
        assert_eq!(button.style(), style);
        assert_eq!(button.command(), &command);
    }

    let status = ChannelAgentStatus::new("reviewer", true).with_button(ChannelButton::new(
        "Disable",
        ChannelButtonStyle::Danger,
        command,
    ));
    assert_eq!(status.name(), "reviewer");
    assert!(status.enabled());
    assert_eq!(status.button().unwrap().text(), "Disable");
    assert_eq!(ChannelAgentStatus::new("codex", false).button(), None);

    assert_eq!(ChannelReply::new("hello").as_text(), Some("hello"));
    assert_eq!(
        ChannelReply::agent_list(vec![status.clone()]).as_text(),
        None
    );
    assert_eq!(ChannelReply::agent_status(status).as_text(), None);

    let callback = InterruptCallback::new(|| true);
    assert!(callback.trigger());
    assert_eq!(format!("{callback:?}"), "InterruptCallback");
}

#[test]
fn configured_channels_reject_unimplemented_types_and_keep_configured_names() {
    for kind in ["local", "http"] {
        assert!(
            serde_json::from_value::<ChannelConfig>(serde_json::json!({"type":kind,"name":"test"}))
                .is_err()
        );
    }
    let lark = ConfiguredChannel::from_config(ChannelConfig::Lark(LarkChannelConfig {
        name: "lark".to_string(),
        app_id: "app-id".to_string(),
        secret: "secret".to_string(),
        permission: Default::default(),
        proxy: None,
    }))
    .unwrap();
    assert_eq!(lark.name(), "lark");

    let telegram = ConfiguredChannel::from_config(ChannelConfig::Telegram(TelegramChannelConfig {
        name: "telegram".to_string(),
        token: "123:secret".to_string(),
        permission: Default::default(),
        proxy: None,
    }))
    .unwrap();
    assert_eq!(telegram.name(), "telegram");
}

#[test]
fn truncated_output_keeps_only_a_bounded_allocation_and_utf8_tail() {
    let input = "界".repeat(700_000);
    let (mut tail, truncated) = bounded_tail(input, 8192, "…");
    assert!(truncated);
    assert!(tail.len() <= 8192 && tail.capacity() <= 8192);
    assert!(tail.starts_with('…') && tail.ends_with('界'));
    assert!(append_bounded_tail(&mut tail, "后缀", 8192, "…"));
    assert!(tail.ends_with("界后缀"));
}

#[test]
fn interrupt_callbacks_are_one_shot_and_removed_with_their_registration() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let callbacks = InterruptCallbacks::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let registration = callbacks.register(InterruptCallback::new(move || {
        callback_calls.fetch_add(1, Ordering::Relaxed);
        true
    }));
    let callback_id = registration.id().to_string();

    assert!(callbacks.trigger(&callback_id));
    assert!(!callbacks.trigger(&callback_id));
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    let registration = callbacks.register(InterruptCallback::new(|| true));
    let callback_id = registration.id().to_string();
    drop(registration);
    assert!(!callbacks.trigger(&callback_id));
}

#[test]
fn interrupt_callback_ids_do_not_repeat_after_restart() {
    let first = InterruptCallbacks::default()
        .register(InterruptCallback::new(|| true))
        .id()
        .to_string();
    let second = InterruptCallbacks::default()
        .register(InterruptCallback::new(|| true))
        .id()
        .to_string();

    assert_ne!(first, second);
}
