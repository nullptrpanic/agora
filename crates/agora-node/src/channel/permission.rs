use crate::config::ChannelPermissionConfig;
use crate::i18n;
use agora_core::logger;
use anyhow::Result;
use serde_json::{Value, json};
use std::future::Future;

const WILDCARD: &str = "*";

#[derive(Clone, Debug)]
pub(super) struct PermissionGate {
    config: ChannelPermissionConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PermissionDenial {
    channel_name: String,
    user_id: String,
    group_id: Option<String>,
    reason: &'static str,
    permission_example: Value,
}

impl PermissionDenial {
    pub(super) fn new(
        channel_name: impl Into<String>,
        user_id: impl Into<String>,
        group_id: Option<&str>,
        reason: &'static str,
    ) -> Self {
        let channel_name = channel_name.into();
        let user_id = user_id.into();
        let permission = match group_id {
            Some(group_id) => json!({
                "users": [{"id": user_id}],
                "groups": [{
                    "id": group_id,
                    "require_mention": false
                }]
            }),
            None => json!({
                "users": [{"id": user_id}]
            }),
        };
        Self {
            channel_name,
            user_id,
            group_id: group_id.map(str::to_string),
            reason,
            permission_example: permission,
        }
    }

    pub(super) fn channel_name(&self) -> &str {
        &self.channel_name
    }

    pub(super) fn user_id(&self) -> &str {
        &self.user_id
    }

    pub(super) fn group_id(&self) -> Option<&str> {
        self.group_id.as_deref()
    }

    pub(super) fn reason(&self) -> &str {
        self.reason
    }

    pub(super) fn configuration_example(&self) -> String {
        let permission = serde_json::to_string_pretty(&self.permission_example)
            .unwrap_or_else(|_| "{}".to_string())
            .replace('\n', "\n      ");
        format!(
            "{{\n  \"channels\": [\n    {{\n      // ...\n      \"permission\": {permission}\n    }}\n  ]\n}}"
        )
    }
}

impl PermissionGate {
    pub(super) fn new(config: ChannelPermissionConfig) -> Self {
        Self { config }
    }

    pub(super) fn authorize(&self, context: &AccessContext<'_>) -> Result<(), DenialReason> {
        if !context.resolved {
            return Err(DenialReason::GroupNotAllowed);
        }
        if !self
            .config
            .users
            .iter()
            .any(|user| user.id == WILDCARD || user.id == context.user_id)
        {
            return Err(DenialReason::UserNotAllowed);
        }
        let Some(group_id) = context.group_id else {
            return Ok(());
        };
        let group = self.group(group_id).ok_or(DenialReason::GroupNotAllowed)?;
        if context.check_mention && group.require_mention && !context.mentioned {
            return Err(DenialReason::MentionRequired);
        }
        Ok(())
    }

    pub(super) async fn admit<F, Fut, T>(
        &self,
        channel_name: &str,
        context: &AccessContext<'_>,
        deliver_denial: F,
    ) -> bool
    where
        F: FnOnce(PermissionDenial) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let Err(reason) = self.authorize(context) else {
            return true;
        };
        if context.is_unmentioned_group_message() {
            return false;
        }
        let reason = match reason {
            DenialReason::UserNotAllowed => i18n::PERMISSION_USER_NOT_ALLOWED,
            DenialReason::GroupNotAllowed => i18n::PERMISSION_GROUP_NOT_ALLOWED,
            DenialReason::MentionRequired => return false,
        };
        let denial =
            PermissionDenial::new(channel_name, context.user_id(), context.group_id(), reason);
        if let Err(err) = deliver_denial(denial).await {
            logger::error!(
                "channel permission denial reply failed channel={} error={}",
                channel_name,
                err
            );
        }
        false
    }

    fn group(&self, group_id: &str) -> Option<&crate::config::ChannelGroupPermissionConfig> {
        self.config
            .groups
            .iter()
            .find(|group| group.id == group_id)
            .or_else(|| self.config.groups.iter().find(|group| group.id == WILDCARD))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AccessContext<'a> {
    user_id: &'a str,
    group_id: Option<&'a str>,
    mentioned: bool,
    check_mention: bool,
    resolved: bool,
}

impl<'a> AccessContext<'a> {
    pub(super) fn private(user_id: &'a str) -> Self {
        Self {
            user_id,
            group_id: None,
            mentioned: false,
            check_mention: false,
            resolved: true,
        }
    }

    pub(super) fn group(user_id: &'a str, group_id: &'a str, mentioned: bool) -> Self {
        Self {
            user_id,
            group_id: Some(group_id),
            mentioned,
            check_mention: true,
            resolved: true,
        }
    }

    pub(super) fn group_action(user_id: &'a str, group_id: &'a str) -> Self {
        Self {
            user_id,
            group_id: Some(group_id),
            mentioned: false,
            check_mention: false,
            resolved: true,
        }
    }

    pub(super) fn unresolved_group(user_id: &'a str, group_id: &'a str) -> Self {
        Self {
            user_id,
            group_id: Some(group_id),
            mentioned: false,
            check_mention: false,
            resolved: false,
        }
    }

    pub(super) fn user_id(&self) -> &str {
        self.user_id
    }

    pub(super) fn group_id(&self) -> Option<&str> {
        self.group_id
    }

    fn is_unmentioned_group_message(&self) -> bool {
        self.group_id.is_some() && self.check_mention && !self.mentioned
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DenialReason {
    UserNotAllowed,
    GroupNotAllowed,
    MentionRequired,
}

#[cfg(test)]
mod tests;
