use super::LarkReplyTarget;
use super::card::{LarkAgentCard, LarkReplyCard};
use super::lark_api::{LarkApi, LarkImageDownloadError};
use crate::channel::permission::{AccessContext, PermissionGate};
use crate::channel::{
    Channel, ChannelDelivery, ChannelReply, ChannelRun, ChannelRunContext, ChannelTask,
    DeliveryDisposition, InterruptCallbacks, RunEvent,
};
#[cfg(test)]
use crate::config::ChannelPermissionConfig;
use crate::config::LarkChannelConfig;
use crate::store::ChannelIdentity;
use crate::task::{ChannelTaskInput, CommandRequest, TaskAttachment, TaskContent};
use agora_core::logger;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const GROUP_SESSION_CAPACITY: usize = 4096;
const LARK_MAX_IMAGES_PER_MESSAGE: usize = 16;
const LARK_EVENT_ADMISSION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
struct GroupSessions {
    entries: HashMap<String, bool>,
    insertion_order: VecDeque<String>,
}

impl GroupSessions {
    fn insert(&mut self, session_id: String, group: bool) {
        if !self.entries.contains_key(&session_id) {
            while self.entries.len() >= GROUP_SESSION_CAPACITY {
                let Some(expired) = self.insertion_order.pop_front() else {
                    break;
                };
                self.entries.remove(&expired);
            }
            self.insertion_order.push_back(session_id.clone());
        }
        self.entries.insert(session_id, group);
    }

    fn get(&self, session_id: &str) -> Option<bool> {
        self.entries.get(session_id).copied()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(super) struct LarkMessageEvent {
    #[serde(rename = "event_id")]
    pub(super) id: String,
    pub(super) message_id: String,
    pub(super) chat_id: String,
    pub(super) chat_type: String,
    pub(super) sender_id: String,
    pub(super) message_type: String,
    pub(super) content: String,
    pub(super) image_keys: Vec<String>,
    pub(super) mention_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LarkEvent {
    Message(LarkMessageEvent),
    CardAction(LarkCardActionEvent),
    Interrupt(LarkInterruptEvent),
    Ignore { event_type: String },
}

#[derive(Debug)]
pub(super) struct LarkDelivery {
    event: LarkEvent,
    acknowledgement: oneshot::Sender<u16>,
    deadline: tokio::time::Instant,
}

impl LarkDelivery {
    pub(super) fn new(event: LarkEvent) -> (Self, oneshot::Receiver<u16>) {
        let (acknowledgement, acknowledged) = oneshot::channel();
        (
            Self {
                event,
                acknowledgement,
                deadline: tokio::time::Instant::now() + LARK_EVENT_ADMISSION_TIMEOUT,
            },
            acknowledged,
        )
    }

    pub(super) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    pub(super) fn into_parts(self) -> (LarkEvent, oneshot::Sender<u16>) {
        (self.event, self.acknowledgement)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LarkCardActionEvent {
    pub(super) id: String,
    pub(super) user_id: String,
    pub(super) session_id: String,
    pub(super) message_id: String,
    pub(super) command: CommandRequest,
    pub(super) conversation: Option<LarkConversation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LarkInterruptEvent {
    pub(super) id: String,
    pub(super) user_id: String,
    pub(super) session_id: String,
    pub(super) message_id: String,
    pub(super) callback_id: String,
    pub(super) conversation: Option<LarkConversation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LarkConversation {
    Private,
    Group,
}

impl LarkConversation {
    fn from_chat_type(chat_type: &str) -> Self {
        if chat_type == "p2p" {
            Self::Private
        } else {
            Self::Group
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
        }
    }

    fn from_action_value(value: &Value) -> Result<Option<Self>> {
        match value.pointer("/event/action/value/agora_conversation") {
            None => Ok(None),
            Some(Value::String(value)) if value == "private" => Ok(Some(Self::Private)),
            Some(Value::String(value)) if value == "group" => Ok(Some(Self::Group)),
            Some(_) => Err(anyhow!(
                "lark card action has an invalid agora conversation"
            )),
        }
    }
}

impl LarkEvent {
    pub(super) fn id(&self) -> Option<&str> {
        match self {
            Self::Message(event) => Some(&event.id),
            Self::CardAction(event) => Some(&event.id),
            Self::Interrupt(event) => Some(&event.id),
            Self::Ignore { .. } => None,
        }
    }

    pub(super) fn from_lark_event_payload(payload: impl AsRef<[u8]>) -> Result<Self> {
        let value: Value = serde_json::from_slice(payload.as_ref())
            .context("lark event payload is not valid json")?;
        let event_type = value
            .pointer("/header/event_type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("lark message event missing header.event_type"))?;
        match event_type {
            "im.message.receive_v1" => {
                LarkMessageEvent::from_lark_event_value(&value).map(Self::Message)
            }
            "card.action.trigger"
                if value
                    .pointer("/event/action/value/agora_interrupt")
                    .is_some() =>
            {
                LarkInterruptEvent::from_lark_event_value(&value).map(Self::Interrupt)
            }
            "card.action.trigger"
                if value.pointer("/event/action/value/agora_command").is_some() =>
            {
                LarkCardActionEvent::from_lark_event_value(&value).map(Self::CardAction)
            }
            _ => Ok(Self::Ignore {
                event_type: event_type.to_string(),
            }),
        }
    }
}

impl LarkInterruptEvent {
    fn from_lark_event_value(value: &Value) -> Result<Self> {
        let required = |path: &str, field: &str| {
            value
                .pointer(path)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("lark interrupt action missing {field}"))
        };
        Ok(Self {
            id: required("/header/event_id", "header.event_id")?,
            user_id: required("/event/operator/open_id", "event.operator.open_id")?,
            session_id: required("/event/context/open_chat_id", "event.context.open_chat_id")?,
            message_id: required(
                "/event/context/open_message_id",
                "event.context.open_message_id",
            )?,
            callback_id: required(
                "/event/action/value/agora_interrupt",
                "event.action.value.agora_interrupt",
            )?,
            conversation: LarkConversation::from_action_value(value)?,
        })
    }
}

impl LarkCardActionEvent {
    fn from_lark_event_value(value: &Value) -> Result<Self> {
        let required = |path: &str, field: &str| {
            value
                .pointer(path)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("lark card action missing {field}"))
        };
        let command = value
            .pointer("/event/action/value/agora_command")
            .cloned()
            .ok_or_else(|| anyhow!("lark card action missing event.action.value.agora_command"))?;
        Ok(Self {
            id: required("/header/event_id", "header.event_id")?,
            user_id: required("/event/operator/open_id", "event.operator.open_id")?,
            session_id: required("/event/context/open_chat_id", "event.context.open_chat_id")?,
            message_id: required(
                "/event/context/open_message_id",
                "event.context.open_message_id",
            )?,
            command: serde_json::from_value(command)
                .context("lark card action has an invalid agora command")?,
            conversation: LarkConversation::from_action_value(value)?,
        })
    }
}

impl LarkMessageEvent {
    fn from_lark_event_value(value: &Value) -> Result<Self> {
        let id = value
            .pointer("/header/event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("lark message event missing header.event_id"))?
            .to_string();
        let message = value
            .pointer("/event/message")
            .ok_or_else(|| anyhow!("lark message event missing event.message"))?;
        let sender_id = value
            .pointer("/event/sender/sender_id/open_id")
            .or_else(|| value.pointer("/event/sender/sender_id/user_id"))
            .or_else(|| value.pointer("/event/sender/sender_id/union_id"))
            .and_then(Value::as_str)
            .filter(|sender_id| !sender_id.is_empty())
            .ok_or_else(|| anyhow!("lark message event missing event.sender.sender_id"))?
            .to_string();
        let message_type = Self::required_str(message, "message_type")?.to_string();
        let raw_content = Self::required_str(message, "content")?;
        let (content, image_keys) = Self::normalize_content(&message_type, raw_content);
        let mention_ids = message
            .get("mentions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|mention| {
                mention
                    .pointer("/id/open_id")
                    .or_else(|| mention.pointer("/id/user_id"))
                    .or_else(|| mention.pointer("/id/union_id"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
            .collect();
        Ok(Self {
            id,
            message_id: Self::required_str(message, "message_id")?.to_string(),
            chat_id: Self::required_str(message, "chat_id")?.to_string(),
            chat_type: Self::required_str(message, "chat_type")?.to_string(),
            sender_id,
            content,
            image_keys,
            mention_ids,
            message_type,
        })
    }

    pub(super) fn session_id(&self) -> &str {
        &self.chat_id
    }

    pub(super) fn input(&self) -> &str {
        &self.content
    }

    pub(super) fn image_keys(&self) -> &[String] {
        &self.image_keys
    }

    pub(super) fn mention_ids(&self) -> &[String] {
        &self.mention_ids
    }

    pub(super) fn reply_target(&self) -> LarkReplyTarget {
        LarkReplyTarget {
            message_id: self.message_id.clone(),
        }
    }

    pub(super) fn is_supported_message(&self) -> bool {
        matches!(self.message_type.as_str(), "text" | "post" | "image")
    }

    fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
        value
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("lark message event missing event.message.{field}"))
    }

    fn normalize_content(message_type: &str, raw_content: &str) -> (String, Vec<String>) {
        match message_type {
            "text" => (
                serde_json::from_str::<Value>(raw_content)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| raw_content.to_string()),
                Vec::new(),
            ),
            "post" => serde_json::from_str::<Value>(raw_content)
                .ok()
                .map(|value| Self::flatten_post_content(&value))
                .unwrap_or_else(|| (raw_content.to_string(), Vec::new())),
            "image" => {
                let image_keys = serde_json::from_str::<Value>(raw_content)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("image_key")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .into_iter()
                    .collect();
                (String::new(), image_keys)
            }
            _ => (raw_content.to_string(), Vec::new()),
        }
    }

    fn flatten_post_content(value: &Value) -> (String, Vec<String>) {
        let lines = value
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_array)
            .collect::<Vec<_>>();
        let text = lines
            .iter()
            .map(|line| {
                line.iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let image_keys = lines
            .iter()
            .flat_map(|line| line.iter())
            .filter_map(|item| item.get("image_key").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        (text, image_keys)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LarkTask {
    source: LarkTaskSource,
    input: ChannelTaskInput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LarkTaskSource {
    Message(LarkMessageEvent),
    CardAction(LarkCardActionEvent),
}

impl LarkTask {
    pub(super) fn from_message(event: LarkMessageEvent, content: TaskContent) -> Self {
        Self {
            source: LarkTaskSource::Message(event),
            input: ChannelTaskInput::Message(content),
        }
    }

    pub(super) fn from_card_action(event: LarkCardActionEvent) -> Self {
        Self {
            input: ChannelTaskInput::Command(event.command.clone()),
            source: LarkTaskSource::CardAction(event),
        }
    }

    fn reply_target(&self) -> Option<LarkReplyTarget> {
        match &self.source {
            LarkTaskSource::Message(event) => Some(event.reply_target()),
            LarkTaskSource::CardAction(_) => None,
        }
    }

    fn conversation(&self) -> Option<LarkConversation> {
        match &self.source {
            LarkTaskSource::Message(event) => {
                Some(LarkConversation::from_chat_type(&event.chat_type))
            }
            LarkTaskSource::CardAction(event) => event.conversation,
        }
    }
}

impl ChannelTask for LarkTask {
    fn task_id(&self) -> &str {
        match &self.source {
            LarkTaskSource::Message(event) => &event.message_id,
            LarkTaskSource::CardAction(event) => &event.id,
        }
    }

    fn session_id(&self) -> &str {
        match &self.source {
            LarkTaskSource::Message(event) => &event.chat_id,
            LarkTaskSource::CardAction(event) => &event.session_id,
        }
    }

    fn input(&self) -> &ChannelTaskInput {
        &self.input
    }
}

#[derive(Clone)]
pub struct LarkRun {
    card: LarkAgentCard,
}

impl ChannelRun for LarkRun {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        self.card.publish(event).await
    }
}

pub struct LarkChannel {
    api: LarkApi,
    identity: ChannelIdentity,
    permission: PermissionGate,
    bot_open_id: Option<String>,
    group_sessions: GroupSessions,
    interrupts: InterruptCallbacks,
    receiver: Option<LarkWebSocketReceiver>,
}

impl Clone for LarkChannel {
    fn clone(&self) -> Self {
        Self {
            api: self.api.clone(),
            identity: self.identity.clone(),
            permission: self.permission.clone(),
            bot_open_id: self.bot_open_id.clone(),
            group_sessions: GroupSessions::default(),
            interrupts: self.interrupts.clone(),
            receiver: None,
        }
    }
}

impl LarkChannel {
    pub fn new(config: LarkChannelConfig) -> Result<Self> {
        let identity = ChannelIdentity::new(config.name.clone(), "lark", config.app_id.clone());
        let permission = PermissionGate::new(config.permission.clone());
        Ok(Self {
            api: LarkApi::new(config)?,
            identity,
            permission,
            bot_open_id: None,
            group_sessions: GroupSessions::default(),
            interrupts: InterruptCallbacks::default(),
            receiver: None,
        })
    }

    #[cfg(test)]
    pub(super) fn with_api(api: LarkApi) -> Self {
        Self::with_api_and_permission(
            api,
            ChannelPermissionConfig {
                users: vec![crate::config::ChannelUserPermissionConfig {
                    id: "*".to_string(),
                }],
                groups: vec![crate::config::ChannelGroupPermissionConfig {
                    id: "*".to_string(),
                    require_mention: false,
                }],
            },
        )
    }

    #[cfg(test)]
    pub(super) fn with_api_and_permission(
        api: LarkApi,
        permission: ChannelPermissionConfig,
    ) -> Self {
        let identity = ChannelIdentity::new(api.name(), "lark", "test");
        Self {
            api,
            identity,
            permission: PermissionGate::new(permission),
            bot_open_id: None,
            group_sessions: GroupSessions::default(),
            interrupts: InterruptCallbacks::default(),
            receiver: None,
        }
    }

    fn receiver(&mut self) -> &mut LarkWebSocketReceiver {
        self.receiver
            .get_or_insert_with(|| LarkWebSocketReceiver::spawn(self.api.clone()))
    }

    async fn message_mentions_bot(&mut self, event: &LarkMessageEvent) -> Result<bool> {
        if event.chat_type == "p2p" || event.mention_ids().is_empty() {
            return Ok(false);
        }
        let mentioned = {
            let bot_open_id = match self.bot_open_id.as_deref() {
                Some(open_id) => open_id,
                None => {
                    self.bot_open_id = Some(self.api.bot_open_id().await?);
                    self.bot_open_id.as_deref().unwrap_or_default()
                }
            };
            event.mention_ids().iter().any(|id| id == bot_open_id)
        };
        Ok(mentioned)
    }

    fn action_conversation(
        &self,
        session_id: &str,
        conversation: Option<LarkConversation>,
    ) -> Option<LarkConversation> {
        conversation.or_else(|| {
            self.group_sessions.get(session_id).map(|group| {
                if group {
                    LarkConversation::Group
                } else {
                    LarkConversation::Private
                }
            })
        })
    }

    async fn admit_action(
        &self,
        user_id: &str,
        session_id: &str,
        message_id: &str,
        conversation: Option<LarkConversation>,
    ) -> bool {
        let context = match self.action_conversation(session_id, conversation) {
            Some(LarkConversation::Private) => AccessContext::private(user_id),
            Some(LarkConversation::Group) => AccessContext::group_action(user_id, session_id),
            None => AccessContext::unresolved_group(user_id, session_id),
        };
        let target = LarkReplyTarget {
            message_id: message_id.to_string(),
        };
        let api = self.api.clone();
        self.permission
            .admit(self.name(), &context, move |denial| async move {
                let token = api.tenant_access_token().await?;
                api.reply_card(&token, &target, &LarkReplyCard::permission_denied(&denial))
                    .await
            })
            .await
    }

    pub(super) async fn task_from_event(&self, event: LarkMessageEvent) -> Result<LarkTask> {
        self.task_from_event_with_attachment_limit(event, crate::http::MAX_TASK_ATTACHMENT_BYTES)
            .await
    }

    async fn task_from_event_with_attachment_limit(
        &self,
        event: LarkMessageEvent,
        maximum_bytes: usize,
    ) -> Result<LarkTask> {
        let mut content = TaskContent::new(event.input());
        let mut remaining_bytes = maximum_bytes;
        if event.image_keys().len() > LARK_MAX_IMAGES_PER_MESSAGE {
            return Err(anyhow::Error::new(LarkImageDownloadError::permanent(
                format!("lark messages may contain at most {LARK_MAX_IMAGES_PER_MESSAGE} images"),
            )));
        }
        if !event.image_keys().is_empty() {
            let token = self
                .api
                .tenant_access_token()
                .await
                .context("get lark token for message images failed")?;
            for (index, image_key) in event.image_keys().iter().enumerate() {
                let image = self
                    .api
                    .download_message_image(&token, &event.message_id, image_key, remaining_bytes)
                    .await
                    .map_err(|error| {
                        let message = error.to_string();
                        anyhow::Error::new(error).context(format!(
                            "download lark message image failed: {image_key}: {message}"
                        ))
                    })?;
                remaining_bytes -= image.data.len();
                let file_name = format!(
                    "lark-image-{}.{}",
                    index + 1,
                    Self::image_extension(&image.media_type)
                );
                content = content.with_attachment(TaskAttachment::image(
                    file_name,
                    image.media_type,
                    image.data,
                ));
            }
        }
        Ok(LarkTask::from_message(event, content))
    }

    fn image_extension(media_type: &str) -> &'static str {
        match media_type {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/bmp" => "bmp",
            "image/tiff" => "tiff",
            "image/heic" => "heic",
            _ => "img",
        }
    }

    async fn handle_event(&mut self, event: LarkEvent) -> Result<Option<LarkTask>> {
        match event {
            LarkEvent::Message(event) if event.is_supported_message() => {
                let mentioned = self.message_mentions_bot(&event).await?;
                let context = if event.chat_type == "p2p" {
                    AccessContext::private(&event.sender_id)
                } else {
                    AccessContext::group(&event.sender_id, &event.chat_id, mentioned)
                };
                let target = event.reply_target();
                let api = self.api.clone();
                if !self
                    .permission
                    .admit(self.name(), &context, move |denial| async move {
                        let token = api.tenant_access_token().await?;
                        api.reply_card(&token, &target, &LarkReplyCard::permission_denied(&denial))
                            .await
                    })
                    .await
                {
                    return Ok(None);
                }
                self.group_sessions
                    .insert(event.chat_id.clone(), event.chat_type != "p2p");
                let session_id = event.session_id().to_string();
                let sender_id = event.sender_id.clone();
                let message_id = event.message_id.clone();
                let task = self.task_from_event(event).await?;
                let (input, input_bytes, attachments) = task.input.receipt_log_fields();
                logger::info!(
                    "lark message received channel={} session={} sender={} message_id={} input={} input_bytes={} attachments={}",
                    self.name(),
                    session_id,
                    sender_id,
                    message_id,
                    input,
                    input_bytes,
                    attachments
                );
                Ok(Some(task))
            }
            LarkEvent::CardAction(mut event) => {
                let conversation = self.action_conversation(&event.session_id, event.conversation);
                if !self
                    .admit_action(
                        &event.user_id,
                        &event.session_id,
                        &event.message_id,
                        conversation,
                    )
                    .await
                {
                    return Ok(None);
                }
                event.conversation = conversation;
                logger::info!(
                    "lark card action received channel={} session={} event_id={}",
                    self.name(),
                    event.session_id,
                    event.id
                );
                Ok(Some(LarkTask::from_card_action(event)))
            }
            LarkEvent::Interrupt(event) => {
                if !self
                    .admit_action(
                        &event.user_id,
                        &event.session_id,
                        &event.message_id,
                        event.conversation,
                    )
                    .await
                {
                    return Ok(None);
                }
                let triggered = self.interrupts.trigger(&event.callback_id);
                logger::info!(
                    "lark interrupt action received channel={} event_id={} triggered={}",
                    self.name(),
                    event.id,
                    triggered
                );
                Ok(None)
            }
            LarkEvent::Message(_) | LarkEvent::Ignore { .. } => Ok(None),
        }
    }
}

impl Channel for LarkChannel {
    type Task = LarkTask;
    type Run = LarkRun;

    fn name(&self) -> &str {
        self.api.name()
    }

    fn identity(&self) -> ChannelIdentity {
        self.identity.clone()
    }

    async fn recv(&mut self) -> Result<Option<ChannelDelivery<Self::Task>>> {
        loop {
            let Some(delivery) = self.receiver().next_delivery().await? else {
                return Ok(None);
            };
            let deadline = delivery.deadline();
            let (event, acknowledgement) = delivery.into_parts();
            let admitted = tokio::time::timeout_at(deadline, self.handle_event(event)).await;
            match admitted {
                Err(_) => {
                    let _ = acknowledgement.send(500);
                    return Err(anyhow!("lark event admission timed out"));
                }
                Ok(Ok(Some(task))) => {
                    return Ok(Some(ChannelDelivery::new(
                        task,
                        deadline,
                        move |disposition| {
                            let status = match disposition {
                                DeliveryDisposition::Accepted => 200,
                                DeliveryDisposition::Retry => 500,
                            };
                            let _ = acknowledgement.send(status);
                        },
                    )));
                }
                Ok(Ok(None)) => {
                    let _ = acknowledgement.send(200);
                }
                Ok(Err(error)) => {
                    let permanent = error
                        .downcast_ref::<LarkImageDownloadError>()
                        .is_some_and(LarkImageDownloadError::is_permanent);
                    let _ = acknowledgement.send(if permanent { 200 } else { 500 });
                    return Err(error);
                }
            }
        }
    }

    async fn open_run(&self, task: &Self::Task, context: ChannelRunContext) -> Result<Self::Run> {
        let target = task
            .reply_target()
            .ok_or_else(|| anyhow!("lark card action cannot open an agent run"))?;
        let interrupt = context
            .interrupt
            .map(|callback| self.interrupts.register(callback));
        let conversation = task
            .conversation()
            .context("lark message conversation is unavailable")?;
        Ok(LarkRun {
            card: LarkAgentCard::new(
                target,
                context.agent.name,
                interrupt,
                conversation,
                self.api.clone(),
            ),
        })
    }

    async fn reply(&self, task: &Self::Task, reply: ChannelReply) -> Result<()> {
        let token = self.api.tenant_access_token().await?;
        match &task.source {
            LarkTaskSource::Message(event) => match reply.as_text() {
                Some(text) => {
                    self.api
                        .reply_text(&token, &event.reply_target(), text)
                        .await
                }
                None => {
                    self.api
                        .reply_card(
                            &token,
                            &event.reply_target(),
                            &LarkReplyCard::build(
                                &reply,
                                LarkConversation::from_chat_type(&event.chat_type),
                            ),
                        )
                        .await?;
                    Ok(())
                }
            },
            LarkTaskSource::CardAction(event) => {
                let conversation = self
                    .action_conversation(&event.session_id, event.conversation)
                    .context("lark card action conversation is unresolved")?;
                self.api
                    .patch_card(
                        &token,
                        &event.message_id,
                        &LarkReplyCard::build(&reply, conversation),
                    )
                    .await
            }
        }
    }
}

struct LarkWebSocketReceiver {
    events: mpsc::Receiver<LarkDelivery>,
    task: Option<JoinHandle<Result<()>>>,
}

impl LarkWebSocketReceiver {
    fn spawn(api: LarkApi) -> Self {
        let (sender, events) = mpsc::channel(1);
        let task = tokio::spawn(async move { api.run_websocket_loop(sender).await });
        Self {
            events,
            task: Some(task),
        }
    }

    async fn next_delivery(&mut self) -> Result<Option<LarkDelivery>> {
        match self.events.recv().await {
            Some(delivery) => Ok(Some(delivery)),
            None => {
                if let Some(task) = self.task.take() {
                    task.await
                        .map_err(|err| anyhow!("lark websocket receiver task failed: {err}"))??;
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests;
