use super::*;
use crate::config::{LarkChannelConfig, NamedChannelConfig, TelegramChannelConfig};

#[test]
fn channel_value_objects_expose_buttons_statuses_replies_and_callbacks() {
    let command = CommandRequest::new(["ask", "enable"]).with_argument("agent_name", "reviewer");
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
    assert!(
        ConfiguredChannel::from_config(ChannelConfig::Local(NamedChannelConfig {
            name: "local".to_string(),
            permission: Default::default(),
            proxy: None,
        }))
        .err()
        .unwrap()
        .to_string()
        .contains("not implemented")
    );
    assert!(
        ConfiguredChannel::from_config(ChannelConfig::Http(NamedChannelConfig {
            name: "http".to_string(),
            permission: Default::default(),
            proxy: None,
        }))
        .err()
        .unwrap()
        .to_string()
        .contains("not implemented")
    );

    let lark = ConfiguredChannel::from_config(ChannelConfig::Lark(LarkChannelConfig {
        name: "lark".to_string(),
        app_id: "app-id".to_string(),
        secret: "secret".to_string(),
        permission: Default::default(),
        proxy: None,
    }))
    .unwrap()
    .unwrap();
    assert_eq!(lark.name(), "lark");

    let telegram = ConfiguredChannel::from_config(ChannelConfig::Telegram(TelegramChannelConfig {
        name: "telegram".to_string(),
        token: "123:secret".to_string(),
        permission: Default::default(),
        proxy: None,
    }))
    .unwrap()
    .unwrap();
    assert_eq!(telegram.name(), "telegram");
}
