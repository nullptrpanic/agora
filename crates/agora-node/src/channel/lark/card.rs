use super::channel::LarkConversation;
use crate::channel::{ChannelAgentStatus, ChannelButton, ChannelButtonStyle, ChannelReply};
use crate::i18n;
use serde_json::{Value, json};

pub(super) struct LarkReplyCard;

impl LarkReplyCard {
    pub(super) fn build(reply: &ChannelReply, conversation: LarkConversation) -> Value {
        let card = match reply {
            ChannelReply::Text(text) | ChannelReply::StopAccepted(text) => Self::card(
                i18n::AGENT_STATUS_TITLE,
                i18n::CURRENT_CONVERSATION.to_string(),
                vec![json!({
                    "tag": "markdown",
                    "content": text
                })],
            ),
            ChannelReply::AgentList(agents) => Self::agent_list(agents, conversation),
            ChannelReply::AgentStatus(agent) => Self::agent_status(agent),
        };
        if card.to_string().len() > super::lark_api::LARK_CARD_MAX_BYTES {
            return Self::card(
                i18n::AGENT_STATUS_TITLE,
                i18n::CURRENT_CONVERSATION.to_string(),
                vec![json!({"tag":"markdown", "content": i18n::AGENT_REPLY_TOO_LARGE})],
            );
        }
        card
    }

    fn agent_list(agents: &[ChannelAgentStatus], conversation: LarkConversation) -> Value {
        let mut elements = Vec::new();
        for (index, agent) in agents.iter().enumerate() {
            if index > 0 {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.push(Self::agent_row(agent, conversation));
        }
        if !elements.is_empty() {
            elements.push(json!({ "tag": "hr" }));
        }
        elements.push(json!({
            "tag": "markdown",
            "content": format!(
                "<font color='grey'>{}</font>",
                i18n::CURRENT_CONVERSATION_ONLY
            )
        }));
        Self::card(
            i18n::AGENT_STATUS_TITLE,
            i18n::agent_count(agents.len()),
            elements,
        )
    }

    fn agent_status(agent: &ChannelAgentStatus) -> Value {
        let (color, state, _) = Self::status_text(agent.enabled());
        Self::card(
            i18n::AGENT_STATUS_TITLE,
            i18n::CURRENT_CONVERSATION.to_string(),
            vec![json!({
                "tag": "column_set",
                "flex_mode": "none",
                "columns": [
                    {
                        "tag": "column",
                        "width": "weighted",
                        "weight": 1,
                        "vertical_align": "center",
                        "elements": [{
                            "tag": "markdown",
                            "content": format!(
                                "**{}**\n<font color='grey'>{}</font>",
                                agent.name(),
                                i18n::MESSAGE_DELIVERY_STATUS
                            )
                        }]
                    },
                    {
                        "tag": "column",
                        "width": "auto",
                        "vertical_align": "center",
                        "elements": [{
                            "tag": "markdown",
                            "content": format!("<font color='{color}'>● {state}</font>"),
                            "text_align": "right"
                        }]
                    }
                ]
            })],
        )
    }

    fn agent_row(agent: &ChannelAgentStatus, conversation: LarkConversation) -> Value {
        let (color, state, description) = Self::status_text(agent.enabled());
        let mut columns = vec![json!({
            "tag": "column",
            "width": "weighted",
            "weight": 1,
            "vertical_align": "center",
            "elements": [{
                "tag": "markdown",
                "content": format!(
                    "**{}**\n<font color='{color}'>{state}</font> · {description}",
                    agent.name()
                )
            }]
        })];
        if let Some(button) = agent.button() {
            columns.push(json!({
                "tag": "column",
                "width": "auto",
                "vertical_align": "center",
                "elements": [Self::button(button, conversation)]
            }));
        }
        json!({
            "tag": "column_set",
            "flex_mode": "none",
            "horizontal_spacing": "default",
            "columns": columns
        })
    }

    fn button(button: &ChannelButton, conversation: LarkConversation) -> Value {
        let button_type = match button.style() {
            ChannelButtonStyle::Default => "default",
            ChannelButtonStyle::Primary => "primary",
            ChannelButtonStyle::Danger => "danger",
        };
        json!({
            "tag": "button",
            "text": {
                "tag": "plain_text",
                "content": button.text()
            },
            "type": button_type,
            "size": "medium",
            "behaviors": [{
                "type": "callback",
                "value": {
                    "agora_command": button.command(),
                    "agora_conversation": conversation.as_str()
                }
            }]
        })
    }

    fn status_text(enabled: bool) -> (&'static str, &'static str, &'static str) {
        if enabled {
            (
                "green",
                i18n::AGENT_ENABLED,
                i18n::AGENT_ENABLED_DESCRIPTION,
            )
        } else {
            (
                "grey",
                i18n::AGENT_DISABLED,
                i18n::AGENT_DISABLED_DESCRIPTION,
            )
        }
    }

    fn card(title: &str, subtitle: String, elements: Vec<Value>) -> Value {
        json!({
            "schema": "2.0",
            "config": {
                "update_multi": true,
                "summary": {
                    "content": title
                }
            },
            "header": {
                "template": "blue",
                "title": {
                    "tag": "plain_text",
                    "content": title
                },
                "subtitle": {
                    "tag": "plain_text",
                    "content": subtitle
                }
            },
            "body": {
                "elements": elements
            }
        })
    }
}

#[cfg(test)]
mod tests;
