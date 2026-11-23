use super::channel::{LarkConversation, LarkMessageEvent};
use crate::task::{ConversationContext, ConversationMention};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MessageReferences {
    thread_id: Option<String>,
    reply_to_message_id: Option<String>,
    pub(super) mentions: Vec<ConversationMention>,
}

impl MessageReferences {
    pub(super) fn from_message(message: &Value) -> Self {
        let identifier = |key: &str| {
            message[key]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .map(str::to_owned)
        };
        let mentions = message["mentions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let id = item.pointer("/id/open_id")?.as_str()?;
                let key = item["key"].as_str()?;
                if id.is_empty() || id.len() > 256 || key.len() > 128 {
                    return None;
                }
                let name = item["name"]
                    .as_str()
                    .filter(|name| !name.is_empty() && name.len() <= 128);
                Some(ConversationMention {
                    text: name.map_or_else(|| key.to_owned(), |name| format!("{key} (@{name})")),
                    id: id.to_owned(),
                })
            })
            .take(16)
            .collect();
        Self {
            thread_id: identifier("thread_id"),
            reply_to_message_id: identifier("parent_id"),
            mentions,
        }
    }
}

impl LarkMessageEvent {
    pub(super) fn conversation_context(&self) -> ConversationContext {
        ConversationContext {
            channel: "lark",
            chat_type: LarkConversation::from_chat_type(&self.chat_type).as_str(),
            chat_id: self.chat_id.clone(),
            message_id: self.message_id.clone(),
            thread_id: self.references.thread_id.clone(),
            reply_to_message_id: self.references.reply_to_message_id.clone(),
            mentions: self.references.mentions.clone(),
        }
    }
}
