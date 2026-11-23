use super::{TelegramMessage, TelegramSender};
use crate::task::{ConversationContext, ConversationMention};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(super) struct Entity {
    #[serde(rename = "type")]
    kind: String,
    offset: usize,
    length: usize,
    user: Option<TelegramSender>,
}

impl Entity {
    fn mention(&self, text: &str, bot_username: &str) -> Option<ConversationMention> {
        if !matches!(self.kind.as_str(), "text_mention" | "mention")
            || self.length == 0
            || self.length > 128
        {
            return None;
        }
        let end = self.offset.checked_add(self.length)?;
        let mut units = 0;
        let mut start = None;
        let mut stop = None;
        for (index, c) in text
            .char_indices()
            .chain(std::iter::once((text.len(), '\0')))
        {
            if units == self.offset {
                start = Some(index);
            }
            if units == end {
                stop = Some(index);
                break;
            }
            units += c.len_utf16();
        }
        let name = text.get(start?..stop?)?;
        if name
            .trim_start_matches('@')
            .eq_ignore_ascii_case(bot_username.trim_start_matches('@'))
        {
            return None;
        }
        let identity = match self.kind.as_str() {
            "text_mention" => {
                let user = self.user.as_ref()?;
                if user.is_bot {
                    return None;
                }
                user.id.to_string()
            }
            _ => name.to_ascii_lowercase(),
        };
        Some(ConversationMention {
            text: name.into(),
            id: identity,
        })
    }
}

impl TelegramMessage {
    fn can_reference(&self) -> bool {
        self.chat.is_supported()
            && self.message_id > 0
            && self.receiver_user.is_none()
            && self.ephemeral_message_id.is_none()
            && !self.has_protected_content
    }

    fn local_parent(&self) -> Option<&Self> {
        self.reply_to_message.as_deref().filter(|p| {
            p.chat.id == self.chat.id
                && p.message_thread_id == self.message_thread_id
                && p.message_id < self.message_id
                && p.date <= self.date
                && p.edit_date.unwrap_or(p.date) <= self.date
                && p.can_reference()
        })
    }

    pub(super) fn conversation_context(&self, bot_username: &str) -> ConversationContext {
        let entities = if self.text.is_some() {
            &self.entities
        } else {
            &self.caption_entities
        };
        let original = self
            .text
            .as_deref()
            .or(self.caption.as_deref())
            .unwrap_or_default();
        let mentions = entities
            .iter()
            .filter_map(|e| e.mention(original, bot_username))
            .take(16)
            .collect();
        ConversationContext {
            channel: "telegram",
            chat_type: if self.chat.is_private() {
                "private"
            } else {
                "group"
            },
            chat_id: self.chat.id.to_string(),
            message_id: self.message_id.to_string(),
            thread_id: self.message_thread_id.map(|id| id.to_string()),
            reply_to_message_id: self.local_parent().map(|p| p.message_id.to_string()),
            mentions,
        }
    }
}
