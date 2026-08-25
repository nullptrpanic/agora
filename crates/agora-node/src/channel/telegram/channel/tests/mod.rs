use super::super::telegram_api::{TelegramApi, TelegramBotCommand};
use super::{TelegramChannel, TelegramInterruptCallbacks, TelegramUpdate};
use crate::channel::permission::PermissionDenial;
use crate::channel::test_http::{HttpMockServer, MockResponse};
use crate::channel::{
    Channel, ChannelAgent, ChannelAgentStatus, ChannelReply, ChannelRun, ChannelRunContext,
    ChannelTask, ConfiguredChannel, ConfiguredTask, InterruptCallback, RunEvent,
};
use crate::config::{
    ChannelConfig, ChannelGroupPermissionConfig, ChannelPermissionConfig,
    ChannelUserPermissionConfig, TelegramChannelConfig,
};

mod api;
mod messages;

fn telegram_config() -> TelegramChannelConfig {
    TelegramChannelConfig {
        name: "telegram-test".to_string(),
        token: "123456:secret".to_string(),
        permission: permission(&["*"], &[("*", false)]),
        proxy: None,
    }
}

fn permission(users: &[&str], groups: &[(&str, bool)]) -> ChannelPermissionConfig {
    ChannelPermissionConfig {
        users: users
            .iter()
            .map(|id| ChannelUserPermissionConfig {
                id: (*id).to_string(),
            })
            .collect(),
        groups: groups
            .iter()
            .map(|(id, require_mention)| ChannelGroupPermissionConfig {
                id: (*id).to_string(),
                require_mention: *require_mention,
            })
            .collect(),
    }
}
