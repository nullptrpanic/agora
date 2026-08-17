use super::rich_message::TelegramRichMessage;
use super::telegram_api::{TelegramApi, TelegramBotCommand, TelegramFileDownloadError};
use crate::channel::permission::{AccessContext, PermissionDenial, PermissionGate};
use crate::channel::{
    CHANNEL_ADMISSION_TIMEOUT, Channel, ChannelDelivery, ChannelReply, ChannelRun,
    ChannelRunContext, ChannelTask, DeliveryDisposition, InterruptCallback, InterruptCallbacks,
    InterruptRegistration, RunEvent,
};
#[cfg(test)]
use crate::config::ChannelPermissionConfig;
use crate::config::TelegramChannelConfig;
use crate::i18n;
use crate::task::{ChannelTaskInput, TaskAttachment, TaskContent};
use agora_core::logger;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use tokio::sync::oneshot;

const TELEGRAM_INTERRUPT_PREFIX: &str = "agora_interrupt:";
const TELEGRAM_IMAGE_MAX_ATTEMPTS: usize = 3;
const TELEGRAM_COMMANDS: &[TelegramBotCommand<'static>] = &[
    TelegramBotCommand::new("stop", i18n::STOP_COMMAND_DESCRIPTION),
    TelegramBotCommand::new("reset", i18n::RESET_COMMAND_DESCRIPTION),
    TelegramBotCommand::new("ask", i18n::ASK_COMMAND_DESCRIPTION),
    TelegramBotCommand::new("help", i18n::HELP_DESCRIPTION),
];

pub struct TelegramChannel {
    api: TelegramApi,
    permission: PermissionGate,
    interrupts: TelegramInterruptCallbacks,
    pending_updates: VecDeque<Value>,
    pending_acknowledgement: Option<(i64, oneshot::Receiver<DeliveryDisposition>)>,
    next_offset: Option<i64>,
    image_retry: Option<(i64, usize)>,
    bot_username: Option<String>,
}

impl Clone for TelegramChannel {
    fn clone(&self) -> Self {
        Self {
            api: self.api.clone(),
            permission: self.permission.clone(),
            interrupts: self.interrupts.clone(),
            pending_updates: VecDeque::new(),
            pending_acknowledgement: None,
            next_offset: None,
            image_retry: None,
            bot_username: None,
        }
    }
}

#[derive(Clone)]
pub struct TelegramRun {
    message: TelegramRichMessage,
}

#[derive(Clone, Default)]
pub(super) struct TelegramInterruptCallbacks {
    callbacks: InterruptCallbacks,
}

impl TelegramInterruptCallbacks {
    fn register(&self, callback: InterruptCallback) -> TelegramInterruptRegistration {
        TelegramInterruptRegistration {
            registration: self.callbacks.register(callback),
        }
    }

    fn trigger(&self, id: &str) -> bool {
        self.callbacks.trigger(id)
    }
}

pub(super) struct TelegramInterruptRegistration {
    registration: InterruptRegistration,
}

impl TelegramInterruptRegistration {
    pub(super) fn callback_data(&self) -> String {
        format!("{TELEGRAM_INTERRUPT_PREFIX}{}", self.registration.id())
    }
}

impl ChannelRun for TelegramRun {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        self.message.publish(event).await
    }
}

impl TelegramChannel {
    pub fn new(config: TelegramChannelConfig) -> Result<Self> {
        let permission = PermissionGate::new(config.permission.clone());
        Ok(Self::with_api_inner(TelegramApi::new(config)?, permission))
    }

    fn with_api_inner(api: TelegramApi, permission: PermissionGate) -> Self {
        Self {
            api,
            permission,
            interrupts: TelegramInterruptCallbacks::default(),
            pending_updates: VecDeque::new(),
            pending_acknowledgement: None,
            next_offset: None,
            image_retry: None,
            bot_username: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_api(api: TelegramApi) -> Self {
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
        api: TelegramApi,
        permission: ChannelPermissionConfig,
    ) -> Self {
        Self::with_api_inner(api, PermissionGate::new(permission))
    }

    async fn next_delivery(&mut self) -> Result<ChannelDelivery<TelegramTask>> {
        loop {
            self.settle_pending_delivery().await;
            self.ensure_bot_username().await?;
            if self.pending_updates.is_empty() {
                self.pending_updates
                    .extend(self.api.get_updates(self.next_offset).await?);
            }
            let Some(value) = self.pending_updates.pop_front() else {
                continue;
            };
            let bot_username = self.bot_username.clone().unwrap_or_default();
            let Some(update_id) = value.get("update_id").and_then(Value::as_i64) else {
                logger::error!(
                    "telegram update ignored channel={} reason=missing_update_id",
                    self.api.name()
                );
                continue;
            };
            match TelegramUpdate::from_value(value) {
                Ok(update) => {
                    debug_assert_eq!(update.update_id(), update_id);
                    if let Some(callback) = update.callback_query() {
                        let access = callback.access();
                        let admitted = if let Some(access) = access.as_ref() {
                            let context = access.context();
                            let target = access.target.clone();
                            let api = self.api.clone();
                            self.permission
                                .admit(self.api.name(), &context, move |denial| async move {
                                    let markdown =
                                        TelegramChannel::render_permission_denial(&denial);
                                    api.send_rich_message(&target, &markdown, None).await
                                })
                                .await
                        } else {
                            false
                        };
                        let interrupt_id = callback
                            .data
                            .as_deref()
                            .and_then(|data| data.strip_prefix(TELEGRAM_INTERRUPT_PREFIX));
                        let triggered =
                            admitted && interrupt_id.is_some_and(|id| self.interrupts.trigger(id));
                        logger::info!(
                            "telegram callback received channel={} update_id={} triggered={}",
                            self.api.name(),
                            update_id,
                            triggered
                        );
                        if access.is_none() {
                            logger::error!(
                                "telegram callback ignored channel={} update_id={} reason=missing_access_identity",
                                self.api.name(),
                                update_id
                            );
                        }
                        self.answer_callback_query(callback.id.clone());
                    } else if let Some(task) = update.into_task(&bot_username) {
                        let context = task.access_context();
                        let target = task.reply_target().clone();
                        let api = self.api.clone();
                        if !self
                            .permission
                            .admit(self.api.name(), &context, move |denial| async move {
                                let markdown = TelegramChannel::render_permission_denial(&denial);
                                api.send_rich_message(&target, &markdown, None).await
                            })
                            .await
                        {
                            self.advance_offset(update_id);
                            continue;
                        }
                        let task = match self.resolve_task_image(task).await {
                            Ok(task) => {
                                self.image_retry = None;
                                task
                            }
                            Err(error)
                                if error.is_retryable() && self.retry_image_update(update_id) =>
                            {
                                self.pending_updates.clear();
                                return Err(error.into());
                            }
                            Err(error) => {
                                logger::error!(
                                    "telegram image update discarded channel={} update_id={} retryable={} error={}",
                                    self.api.name(),
                                    update_id,
                                    error.is_retryable(),
                                    error
                                );
                                self.image_retry = None;
                                self.advance_offset(update_id);
                                continue;
                            }
                        };
                        let (input, input_bytes, attachments) = task.input.receipt_log_fields();
                        logger::info!(
                            "telegram message received channel={} session={} message_id={} input={} input_bytes={} attachments={}",
                            self.api.name(),
                            task.session_id(),
                            task.reply_target.message_id,
                            input,
                            input_bytes,
                            attachments
                        );
                        return Ok(self.defer_task(update_id, task));
                    }
                }
                Err(err) => logger::error!(
                    "telegram update ignored channel={} update_id={} error={}",
                    self.api.name(),
                    update_id,
                    err
                ),
            }
            self.advance_offset(update_id);
        }
    }

    #[cfg(test)]
    pub(super) async fn next_task(&mut self) -> Result<TelegramTask> {
        let (task, receipt) = self.next_delivery().await?.into_parts();
        receipt.accept();
        Ok(task)
    }

    async fn settle_pending_delivery(&mut self) {
        let Some((update_id, acknowledged)) = self.pending_acknowledgement.take() else {
            return;
        };
        match acknowledged.await.unwrap_or(DeliveryDisposition::Retry) {
            DeliveryDisposition::Accepted => self.advance_offset(update_id),
            DeliveryDisposition::Retry => {
                self.pending_updates.clear();
                logger::info!(
                    "telegram delivery scheduled for retry channel={} update_id={}",
                    self.api.name(),
                    update_id
                );
            }
        }
    }

    fn defer_task(&mut self, update_id: i64, task: TelegramTask) -> ChannelDelivery<TelegramTask> {
        let (acknowledgement, acknowledged) = oneshot::channel();
        self.pending_acknowledgement = Some((update_id, acknowledged));
        ChannelDelivery::new(
            task,
            tokio::time::Instant::now() + CHANNEL_ADMISSION_TIMEOUT,
            move |disposition| {
                let _ = acknowledgement.send(disposition);
            },
        )
    }

    async fn ensure_bot_username(&mut self) -> Result<()> {
        if self.bot_username.is_some() {
            return Ok(());
        }
        logger::info!("telegram channel connecting channel={}", self.api.name());
        let username = self.api.bot_username().await?;
        self.bot_username = Some(username.clone());
        if let Err(err) = self.api.set_commands(TELEGRAM_COMMANDS).await {
            logger::error!(
                "telegram command registration failed channel={} error={}",
                self.api.name(),
                err
            );
        }
        logger::info!(
            "telegram channel connected channel={} bot=@{}",
            self.api.name(),
            username
        );
        Ok(())
    }

    async fn resolve_task_image(
        &self,
        mut task: TelegramTask,
    ) -> std::result::Result<TelegramTask, TelegramFileDownloadError> {
        let Some(file_id) = task.image_file_id.take() else {
            return Ok(task);
        };
        let image = self
            .api
            .download_file(&file_id, crate::http::MAX_TASK_ATTACHMENT_BYTES)
            .await?;
        if let ChannelTaskInput::Message(content) = &mut task.input {
            *content = std::mem::take(content).with_attachment(TaskAttachment::image(
                image.file_name,
                image.media_type,
                image.data,
            ));
        }
        Ok(task)
    }

    fn retry_image_update(&mut self, update_id: i64) -> bool {
        let attempts = match self.image_retry {
            Some((current, attempts)) if current == update_id => attempts.saturating_add(1),
            _ => 1,
        };
        self.image_retry = Some((update_id, attempts));
        attempts < TELEGRAM_IMAGE_MAX_ATTEMPTS
    }

    fn advance_offset(&mut self, update_id: i64) {
        let next = update_id.saturating_add(1);
        self.next_offset = Some(self.next_offset.map_or(next, |current| current.max(next)));
    }

    fn answer_callback_query(&self, query_id: String) {
        let api = self.api.clone();
        tokio::spawn(async move {
            if let Err(err) = api.answer_callback_query(&query_id).await {
                logger::error!(
                    "telegram callback acknowledgement failed channel={} error={}",
                    api.name(),
                    err
                );
            }
        });
    }

    pub(super) fn render_reply(reply: &ChannelReply) -> String {
        match reply {
            ChannelReply::Text(text) => text.clone(),
            ChannelReply::AgentList(agents) => {
                let mut sections = vec![format!(
                    "**{}**\n> {}",
                    i18n::AGENT_STATUS_TITLE,
                    i18n::CURRENT_CONVERSATION_ONLY
                )];
                sections.extend(agents.iter().map(Self::render_agent_status));
                sections.join("\n\n")
            }
            ChannelReply::AgentStatus(agent) => format!(
                "**{}**\n> {}\n\n{}",
                i18n::AGENT_STATUS_TITLE,
                i18n::CURRENT_CONVERSATION_ONLY,
                Self::render_agent_status(agent)
            ),
        }
    }

    pub(super) fn render_permission_denial(denial: &PermissionDenial) -> String {
        let mut identifiers = vec![
            format!("- Channel：`{}`", denial.channel_name()),
            format!("- User ID：`{}`", denial.user_id()),
        ];
        if let Some(group_id) = denial.group_id() {
            identifiers.push(format!("- Group ID：`{group_id}`"));
        }
        let configuration = denial.configuration_example();
        format!(
            "**{}**\n\n> {}\n\n**{}**\n{}\n\n**{}**\n```jsonc\n{}\n```",
            i18n::PERMISSION_DENIED_TITLE,
            denial.reason(),
            i18n::PERMISSION_IDENTIFIERS_TITLE,
            identifiers.join("\n"),
            i18n::PERMISSION_CONFIG_EXAMPLE_TITLE,
            configuration
        )
    }

    fn render_agent_status(agent: &crate::channel::ChannelAgentStatus) -> String {
        let (marker, state, description) = if agent.enabled() {
            ("🟢", i18n::AGENT_ENABLED, i18n::AGENT_ENABLED_DESCRIPTION)
        } else {
            ("⚪", i18n::AGENT_DISABLED, i18n::AGENT_DISABLED_DESCRIPTION)
        };
        format!(
            "{marker} **{}** · {state}\n{description}",
            Self::escape_structural_text(agent.name())
        )
    }

    fn escape_structural_text(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

impl Channel for TelegramChannel {
    type Task = TelegramTask;
    type Run = TelegramRun;

    fn name(&self) -> &str {
        self.api.name()
    }

    async fn recv(&mut self) -> Result<Option<ChannelDelivery<Self::Task>>> {
        self.next_delivery().await.map(Some)
    }

    async fn open_run(&self, task: &Self::Task, context: ChannelRunContext) -> Result<Self::Run> {
        let interrupt = context
            .interrupt
            .map(|callback| self.interrupts.register(callback));
        Ok(TelegramRun {
            message: TelegramRichMessage::new(
                task.reply_target().clone(),
                context.agent.name,
                interrupt,
                self.api.clone(),
            ),
        })
    }

    async fn reply(&self, task: &Self::Task, reply: ChannelReply) -> Result<()> {
        self.api
            .send_rich_message(task.reply_target(), &Self::render_reply(&reply), None)
            .await?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TelegramReplyTarget {
    pub(super) chat_id: i64,
    pub(super) message_id: i64,
    pub(super) message_thread_id: Option<i64>,
    pub(super) is_private: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelegramTask {
    task_id: String,
    session_id: String,
    input: ChannelTaskInput,
    reply_target: TelegramReplyTarget,
    image_file_id: Option<String>,
    sender_id: String,
    group_id: Option<String>,
    mentioned_bot: bool,
}

impl TelegramTask {
    pub(super) fn reply_target(&self) -> &TelegramReplyTarget {
        &self.reply_target
    }

    fn access_context(&self) -> AccessContext<'_> {
        match self.group_id.as_deref() {
            Some(group_id) => AccessContext::group(&self.sender_id, group_id, self.mentioned_bot),
            None => AccessContext::private(&self.sender_id),
        }
    }
}

impl ChannelTask for TelegramTask {
    fn task_id(&self) -> &str {
        &self.task_id
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn input(&self) -> &ChannelTaskInput {
        &self.input
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(super) struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

impl TelegramUpdate {
    #[cfg(test)]
    pub(super) fn from_json(payload: &str) -> Result<Self> {
        serde_json::from_str(payload).context("telegram update is not valid json")
    }

    pub(super) fn from_value(value: Value) -> Result<Self> {
        serde_json::from_value(value).context("telegram update has an invalid shape")
    }

    pub(super) fn update_id(&self) -> i64 {
        self.update_id
    }

    fn callback_query(&self) -> Option<&TelegramCallbackQuery> {
        self.callback_query.as_ref()
    }

    pub(super) fn into_task(self, bot_username: &str) -> Option<TelegramTask> {
        let message = self.message?;
        if !message.chat.is_supported() {
            return None;
        }
        let mentioned_bot = message.mentions_bot(bot_username);
        let text = message.normalized_text(bot_username)?;
        let image_file_id = message.photo.last().map(|photo| photo.file_id.clone());
        let sender_id = message.from.as_ref()?.id.to_string();
        let group_id = (!message.chat.is_private()).then(|| message.chat.id.to_string());
        let session_id = match message.message_thread_id {
            Some(thread_id) => {
                format!("chat:{}:topic:{thread_id}", message.chat.id)
            }
            None => format!("chat:{}", message.chat.id),
        };
        let reply_target = TelegramReplyTarget {
            chat_id: message.chat.id,
            message_id: message.message_id,
            message_thread_id: message.message_thread_id,
            is_private: message.chat.is_private(),
        };
        Some(TelegramTask {
            task_id: self.update_id.to_string(),
            session_id,
            input: ChannelTaskInput::Message(TaskContent::new(text)),
            reply_target,
            image_file_id,
            sender_id,
            group_id,
            mentioned_bot,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct TelegramCallbackQuery {
    id: String,
    #[serde(default)]
    from: Option<TelegramSender>,
    #[serde(default)]
    message: Option<TelegramCallbackMessage>,
    #[serde(default)]
    data: Option<String>,
}

impl TelegramCallbackQuery {
    fn access(&self) -> Option<TelegramCallbackAccess> {
        let sender_id = self.from.as_ref()?.id.to_string();
        let message = self.message.as_ref()?;
        let group_id = (!message.chat.is_private()).then(|| message.chat.id.to_string());
        Some(TelegramCallbackAccess {
            sender_id,
            group_id,
            target: TelegramReplyTarget {
                chat_id: message.chat.id,
                message_id: message.message_id,
                message_thread_id: message.message_thread_id,
                is_private: message.chat.is_private(),
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct TelegramCallbackMessage {
    message_id: i64,
    #[serde(default)]
    message_thread_id: Option<i64>,
    chat: TelegramChat,
}

struct TelegramCallbackAccess {
    sender_id: String,
    group_id: Option<String>,
    target: TelegramReplyTarget,
}

impl TelegramCallbackAccess {
    fn context(&self) -> AccessContext<'_> {
        match self.group_id.as_deref() {
            Some(group_id) => AccessContext::group_action(&self.sender_id, group_id),
            None => AccessContext::private(&self.sender_id),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct TelegramMessage {
    message_id: i64,
    #[serde(default)]
    from: Option<TelegramSender>,
    #[serde(default)]
    message_thread_id: Option<i64>,
    chat: TelegramChat,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Vec<TelegramPhotoSize>,
}

impl TelegramMessage {
    fn mentions_bot(&self, bot_username: &str) -> bool {
        self.text
            .as_deref()
            .or(self.caption.as_deref())
            .is_some_and(|text| Self::contains_mention(text, bot_username))
    }

    fn contains_mention(text: &str, bot_username: &str) -> bool {
        let expected = bot_username.trim_start_matches('@');
        text.char_indices()
            .filter(|(_, character)| *character == '@')
            .any(|(index, _)| {
                let username = text[index + 1..]
                    .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                    .next()
                    .unwrap_or_default();
                !username.is_empty() && username.eq_ignore_ascii_case(expected)
            })
    }

    fn normalized_text(&self, bot_username: &str) -> Option<String> {
        let Some(text) = self.text.as_ref().or(self.caption.as_ref()) else {
            return (!self.photo.is_empty()).then(String::new);
        };
        if text.trim().is_empty() {
            return (!self.photo.is_empty()).then(String::new);
        }
        let command_end = text.find(char::is_whitespace).unwrap_or(text.len());
        let (command, suffix) = text.split_at(command_end);
        if !command.starts_with('/') {
            return Some(text.clone());
        }
        let Some((command, target)) = command.split_once('@') else {
            return Some(text.clone());
        };
        if !target.eq_ignore_ascii_case(bot_username.trim_start_matches('@')) {
            return None;
        }
        Some(format!("{command}{suffix}"))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct TelegramSender {
    id: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct TelegramPhotoSize {
    file_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

impl TelegramChat {
    fn is_private(&self) -> bool {
        self.kind == "private"
    }

    fn is_supported(&self) -> bool {
        matches!(self.kind.as_str(), "private" | "group" | "supergroup")
    }
}

#[cfg(test)]
mod tests;
