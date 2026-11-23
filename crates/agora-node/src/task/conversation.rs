use serde::Serialize;

/// Native chat and message identifiers supplied by the channel for an admitted message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConversationContext {
    pub(crate) channel: &'static str,
    pub(crate) chat_type: &'static str,
    pub(crate) chat_id: String,
    pub(crate) message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reply_to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) mentions: Vec<ConversationMention>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ConversationMention {
    pub(crate) text: String,
    pub(crate) id: String,
}
