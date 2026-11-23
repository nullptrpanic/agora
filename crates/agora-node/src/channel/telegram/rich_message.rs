use super::channel::{TelegramInterruptRegistration, TelegramReplyTarget};
use super::telegram_api::TelegramApi;
use crate::channel::{
    ChannelRun, IMAGE_DELIVERY_TIMEOUT, ImageDeliveries, RunEvent, append_bounded_tail,
    bounded_tail, command_output_preview,
};
use crate::i18n::{self, RunStatus};
use crate::task::{OutputEvent, OutputImage, ProgressStatus};
use agora_core::logger;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

mod action_title;

const TELEGRAM_UPDATE_INTERVAL: Duration = Duration::from_millis(400);
const TELEGRAM_DRAFT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const TELEGRAM_DELIVERY_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const TELEGRAM_DELIVERY_MAX_FAILURES: u32 = 3;
const TELEGRAM_RICH_MESSAGE_MAX_CHARS: usize = 32_768;
const TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS: usize = 400;
const TELEGRAM_PROCESS_MAX_CHARS: usize = 20_000;
const TELEGRAM_PROCESS_MAX_STRUCTURE_POINTS: usize = 240;
const TELEGRAM_RETAINED_ANSWER_MAX_BYTES: usize = 256 * 1024;
const TELEGRAM_RETAINED_PROCESS_ENTRIES: usize = 64;
const TELEGRAM_RETAINED_PROCESS_TEXT_MAX_BYTES: usize = 8 * 1024;

#[derive(Clone)]
pub(super) struct TelegramRichMessage {
    inner: Arc<TelegramRichMessageInner>,
}

struct TelegramRichMessageInner {
    target: TelegramReplyTarget,
    interrupt: Option<TelegramInterruptRegistration>,
    api: TelegramApi,
    timing: TelegramRichTiming,
    state: Mutex<TelegramRichMessageState>,
    delivery_lock: Mutex<()>,
}

struct TelegramRichMessageState {
    images: ImageDeliveries,
    content: TelegramRichContent,
    draft_id: i64,
    message_ids: Vec<i64>,
    unconfirmed_part: Option<usize>,
    version: u64,
    sent_version: u64,
    last_update: Option<Instant>,
    flush_scheduled: bool,
    heartbeat_started: bool,
    terminal_sent: bool,
    delivery_failures: u32,
    retry_scheduled: bool,
}

#[derive(Clone, Copy)]
pub(super) struct TelegramRichTiming {
    update_interval: Duration,
    heartbeat_interval: Duration,
    retry_interval: Duration,
}

impl TelegramRichTiming {
    #[cfg(test)]
    pub(super) fn new(update_interval: Duration, heartbeat_interval: Duration) -> Self {
        Self {
            update_interval,
            heartbeat_interval,
            retry_interval: update_interval,
        }
    }
}

impl Default for TelegramRichTiming {
    fn default() -> Self {
        Self {
            update_interval: TELEGRAM_UPDATE_INTERVAL,
            heartbeat_interval: TELEGRAM_DRAFT_HEARTBEAT_INTERVAL,
            retry_interval: TELEGRAM_DELIVERY_RETRY_INTERVAL,
        }
    }
}

impl TelegramRichMessage {
    pub(super) fn new(
        target: TelegramReplyTarget,
        agent_name: String,
        interrupt: Option<TelegramInterruptRegistration>,
        api: TelegramApi,
    ) -> Self {
        Self::with_timing_inner(
            target,
            agent_name,
            interrupt,
            api,
            TelegramRichTiming::default(),
        )
    }

    fn with_timing_inner(
        target: TelegramReplyTarget,
        agent_name: String,
        interrupt: Option<TelegramInterruptRegistration>,
        api: TelegramApi,
        timing: TelegramRichTiming,
    ) -> Self {
        let draft_id = api.allocate_draft_id();
        Self {
            inner: Arc::new(TelegramRichMessageInner {
                target,
                interrupt,
                api,
                timing,
                state: Mutex::new(TelegramRichMessageState {
                    images: ImageDeliveries::default(),
                    content: TelegramRichContent::new(agent_name),
                    draft_id,
                    message_ids: Vec::new(),
                    unconfirmed_part: None,
                    version: 0,
                    sent_version: 0,
                    last_update: None,
                    flush_scheduled: false,
                    heartbeat_started: false,
                    terminal_sent: false,
                    delivery_failures: 0,
                    retry_scheduled: false,
                }),
                delivery_lock: Mutex::new(()),
            }),
        }
    }

    #[cfg(test)]
    pub(super) fn with_timing(
        target: TelegramReplyTarget,
        agent_name: String,
        api: TelegramApi,
        timing: TelegramRichTiming,
    ) -> Self {
        Self::with_timing_inner(target, agent_name, None, api, timing)
    }

    async fn publish_image(&self, id: String, image: OutputImage) -> Result<()> {
        let admitted = {
            let mut state = self.inner.state.lock().await;
            if state.content.is_terminal() {
                return Ok(());
            }
            state.images.admit(&id, &image)
        };
        if matches!(admitted, Ok(false)) {
            return Ok(());
        }
        let result = match admitted {
            Ok(_) => {
                self.publish_event(RunEvent::Output(OutputEvent::Image {
                    id: id.clone(),
                    image: image.clone(),
                }))
                .await?;
                tokio::time::timeout(
                    IMAGE_DELIVERY_TIMEOUT,
                    self.inner.api.send_photo(&self.inner.target, &image),
                )
                .await
                .context("telegram image delivery timed out")
                .and_then(|result| result)
            }
            Err(error) => Err(error),
        };
        let (text, status) = match result {
            Ok(()) => (i18n::IMAGE_SENT, ProgressStatus::Completed),
            Err(error) => {
                logger::error!(
                    "telegram image delivery not confirmed channel={}: {}",
                    self.inner.api.name(),
                    error
                );
                (i18n::IMAGE_SEND_FAILED, ProgressStatus::Failed)
            }
        };
        self.publish_event(RunEvent::Output(OutputEvent::Progress {
            id,
            text: text.to_string(),
            status,
        }))
        .await
    }

    async fn publish_event(&self, event: RunEvent) -> Result<()> {
        let flush_now = {
            let mut state = self.inner.state.lock().await;
            if state.content.is_terminal() {
                if state.terminal_sent {
                    return Ok(());
                }
                true
            } else {
                let flush_now = !matches!(event, RunEvent::Output(_));
                if !state.content.apply(event) {
                    return Ok(());
                }
                state.version = state.version.saturating_add(1);
                if !flush_now && !state.flush_scheduled {
                    state.flush_scheduled = true;
                    let delay = state
                        .last_update
                        .map(|last_update| {
                            self.inner
                                .timing
                                .update_interval
                                .saturating_sub(last_update.elapsed())
                        })
                        .unwrap_or_default();
                    self.schedule_flush(delay);
                }
                flush_now
            }
        };

        if flush_now {
            let result = self.flush_latest(false).await;
            if let Err(err) = &result {
                self.handle_flush_failure("publish", err).await;
            }
            result?;
        }
        Ok(())
    }

    fn schedule_flush(&self, delay: Duration) {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut delay = delay;
            loop {
                tokio::time::sleep(delay).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let message = TelegramRichMessage { inner };
                let attempted_version = message.inner.state.lock().await.version;
                if let Err(err) = message.flush_latest(false).await {
                    message.handle_flush_failure("update", &err).await;
                }
                let mut state = message.inner.state.lock().await;
                if state.version == state.sent_version
                    || state.version == attempted_version
                    || state.content.is_terminal()
                {
                    state.flush_scheduled = false;
                    return;
                }
                delay = message.inner.timing.update_interval;
            }
        });
    }

    fn schedule_heartbeat(&self) {
        let weak = Arc::downgrade(&self.inner);
        let interval = self.inner.timing.heartbeat_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let message = TelegramRichMessage { inner };
                let active = {
                    let state = message.inner.state.lock().await;
                    message.inner.target.is_private && !state.content.is_terminal()
                };
                if !active {
                    return;
                }
                if let Err(err) = message.flush_latest(true).await {
                    message.handle_flush_failure("draft refresh", &err).await;
                }
            }
        });
    }

    async fn flush_latest(&self, force_private_draft: bool) -> Result<()> {
        let _delivery = self.inner.delivery_lock.lock().await;
        let TelegramPendingFlush {
            version,
            messages,
            action,
            terminal,
            callback_data,
        } = {
            let state = self.inner.state.lock().await;
            let terminal = state.content.is_terminal();
            if terminal && state.terminal_sent {
                return Ok(());
            }
            if !terminal && !force_private_draft && state.version == state.sent_version {
                return Ok(());
            }

            let callback_data = (!terminal)
                .then(|| {
                    self.inner
                        .interrupt
                        .as_ref()
                        .map(TelegramInterruptRegistration::callback_data)
                })
                .flatten();
            let action = if self.inner.target.is_private {
                if terminal {
                    state
                        .message_ids
                        .first()
                        .copied()
                        .map(|message_id| TelegramFlushAction::Edit { message_id })
                        .unwrap_or(TelegramFlushAction::Send)
                } else if callback_data.is_some() {
                    state
                        .message_ids
                        .first()
                        .copied()
                        .map(|message_id| TelegramFlushAction::Edit { message_id })
                        .unwrap_or(TelegramFlushAction::Send)
                } else {
                    TelegramFlushAction::Draft {
                        draft_id: state.draft_id,
                    }
                }
            } else if let Some(message_id) = state.message_ids.first().copied() {
                TelegramFlushAction::Edit { message_id }
            } else {
                TelegramFlushAction::Send
            };
            let draft = matches!(action, TelegramFlushAction::Draft { .. });
            TelegramPendingFlush {
                version: state.version,
                messages: state
                    .content
                    .render_messages(draft, self.inner.target.mention.as_deref()),
                action,
                terminal,
                callback_data,
            }
        };

        let primary = messages
            .first()
            .expect("telegram rich content must render at least one message");
        match action {
            TelegramFlushAction::Draft { draft_id } => {
                debug_assert_eq!(messages.len(), 1);
                self.inner
                    .api
                    .send_rich_message_draft(&self.inner.target, draft_id, primary)
                    .await?;
            }
            TelegramFlushAction::Send => {
                self.send_part(0, primary, callback_data.as_deref()).await?;
            }
            TelegramFlushAction::Edit { message_id } => {
                self.inner
                    .api
                    .edit_rich_message(
                        self.inner.target.chat_id,
                        message_id,
                        primary,
                        callback_data.as_deref(),
                    )
                    .await?;
            }
        }

        for (index, markdown) in messages.iter().enumerate().skip(1) {
            let message_id = {
                let state = self.inner.state.lock().await;
                state.message_ids.get(index).copied()
            };
            if let Some(message_id) = message_id {
                self.inner
                    .api
                    .edit_rich_message(self.inner.target.chat_id, message_id, markdown, None)
                    .await?;
            } else {
                self.send_part(index, markdown, None).await?;
            }
        }

        let start_heartbeat = {
            let mut state = self.inner.state.lock().await;
            if terminal {
                state.terminal_sent = true;
            }
            state.sent_version = state.sent_version.max(version);
            state.last_update = Some(Instant::now());
            state.delivery_failures = 0;
            let start_heartbeat = self.inner.target.is_private
                && self.inner.interrupt.is_none()
                && !state.content.is_terminal()
                && !state.heartbeat_started;
            if start_heartbeat {
                state.heartbeat_started = true;
            }
            start_heartbeat
        };
        if start_heartbeat {
            self.schedule_heartbeat();
        }
        Ok(())
    }

    // The delivery lock serializes parts. Reserve this index before network I/O
    // so cancellation or loss of a successful response cannot resend it.
    async fn send_part(&self, index: usize, markdown: &str, callback: Option<&str>) -> Result<()> {
        {
            let mut state = self.inner.state.lock().await;
            anyhow::ensure!(
                state.unconfirmed_part.is_none(),
                "telegram delivery of part {} is unconfirmed",
                index + 1
            );
            state.unconfirmed_part = Some(index);
        }
        let result = self
            .inner
            .api
            .send_rich_message(&self.inner.target, markdown, callback)
            .await;
        let mut state = self.inner.state.lock().await;
        match result {
            Ok(message_id) => {
                debug_assert_eq!(state.message_ids.len(), index);
                state.message_ids.push(message_id);
                state.unconfirmed_part = None;
                Ok(())
            }
            Err(error) => {
                if TelegramApi::send_was_rejected(&error) {
                    state.unconfirmed_part = None;
                }
                Err(error)
            }
        }
    }

    async fn handle_flush_failure(&self, operation: &str, err: &anyhow::Error) {
        let retry = {
            let mut state = self.inner.state.lock().await;
            let terminal = state.content.is_terminal();
            let pending = if terminal {
                !state.terminal_sent
            } else {
                state.version != state.sent_version
            };
            let retryable = !state.message_ids.is_empty()
                || (!terminal && self.inner.target.is_private && self.inner.interrupt.is_none());
            if !pending || !retryable || state.unconfirmed_part.is_some() {
                None
            } else if !state.retry_scheduled {
                state.delivery_failures = state.delivery_failures.saturating_add(1);
                if state.delivery_failures > TELEGRAM_DELIVERY_MAX_FAILURES {
                    logger::error!(
                        "telegram delivery retry budget exhausted chat_id={} terminal={}",
                        self.inner.target.chat_id,
                        terminal
                    );
                    None
                } else {
                    state.retry_scheduled = true;
                    let multiplier = 1_u32 << state.delivery_failures.saturating_sub(1).min(3);
                    Some(self.inner.timing.retry_interval.saturating_mul(multiplier))
                }
            } else {
                None
            }
        };
        logger::error!(
            "telegram rich message {} failed chat_id={} retry_newly_scheduled={} error={}",
            operation,
            self.inner.target.chat_id,
            retry.is_some(),
            err
        );
        if let Some(delay) = retry {
            self.schedule_retry(delay);
        }
    }

    fn schedule_retry(&self, delay: Duration) {
        let message = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let result = message.flush_latest(false).await;
            {
                let mut state = message.inner.state.lock().await;
                state.retry_scheduled = false;
            }
            if let Err(err) = result {
                message.handle_flush_failure("retry", &err).await;
            }
        });
    }
}

enum TelegramFlushAction {
    Draft { draft_id: i64 },
    Send,
    Edit { message_id: i64 },
}

struct TelegramPendingFlush {
    version: u64,
    messages: Vec<String>,
    action: TelegramFlushAction,
    terminal: bool,
    callback_data: Option<String>,
}

impl ChannelRun for TelegramRichMessage {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        if let RunEvent::Output(OutputEvent::Image { id, image }) = event {
            return self.publish_image(id, image).await;
        }
        self.publish_event(event).await
    }
}

pub(super) struct TelegramRichContent {
    agent_name: String,
    process: VecDeque<TelegramProgressEntry>,
    process_truncated: bool,
    answer: String,
    state: TelegramRunState,
}

enum TelegramRunState {
    Queued { ahead: usize },
    Running,
    Completed,
    Failed(String),
    Stopped,
    Interrupted,
}

struct TelegramProgressEntry {
    id: String,
    text: String,
    status: ProgressStatus,
    kind: TelegramProgressKind,
    exit_code: Option<i32>,
    output: Option<(String, Option<u64>)>,
}

impl TelegramProgressEntry {
    fn title(&self) -> String {
        let title = if self.kind == TelegramProgressKind::Command {
            action_title::shell_action_title(&self.text)
        } else {
            i18n::tool_title(&self.text)
        };
        TelegramRichContent::escape_structural_text(&title)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TelegramProgressKind {
    Commentary,
    Progress,
    Tool,
    Command,
}

impl TelegramProgressKind {
    fn is_tool(self) -> bool {
        matches!(self, Self::Tool | Self::Command)
    }
}

impl TelegramRichContent {
    pub(super) fn new(agent_name: String) -> Self {
        Self {
            agent_name: bounded_tail(agent_name, 128, "…").0,
            process: VecDeque::new(),
            process_truncated: false,
            answer: String::new(),
            state: TelegramRunState::Running,
        }
    }

    pub(super) fn apply(&mut self, event: RunEvent) -> bool {
        if self.is_terminal() {
            return false;
        }
        match event {
            RunEvent::Queued { ahead } => self.state = TelegramRunState::Queued { ahead },
            RunEvent::Started { .. } => self.state = TelegramRunState::Running,
            RunEvent::Output(output) => return self.apply_output(output),
            RunEvent::Completed { .. } => self.state = TelegramRunState::Completed,
            RunEvent::Failed { message } => self.state = TelegramRunState::Failed(message),
            RunEvent::Stopped => self.state = TelegramRunState::Stopped,
            RunEvent::Interrupted => self.state = TelegramRunState::Interrupted,
        }
        true
    }

    #[cfg(test)]
    pub(super) fn render(&self, draft: bool) -> String {
        let rendered = self.render_full(draft);
        if Self::within_limits(&rendered) {
            rendered
        } else {
            self.render_truncated(draft)
        }
    }

    pub(super) fn render_messages(&self, draft: bool, mention: Option<&str>) -> Vec<String> {
        let mut sections = self.render_sections(draft);
        if self.is_terminal()
            && !matches!(self.state, TelegramRunState::Stopped)
            && let Some(mention) = mention
        {
            sections.insert(0, mention.to_owned());
        }
        let rendered = sections.join("\n\n");
        if Self::within_limits(&rendered) {
            vec![rendered]
        } else if self.is_terminal() && !draft {
            Self::split_sections(sections)
        } else {
            vec![self.render_truncated(draft)]
        }
    }

    #[cfg(test)]
    fn render_full(&self, draft: bool) -> String {
        self.render_sections(draft).join("\n\n")
    }

    fn render_sections(&self, draft: bool) -> Vec<String> {
        if matches!(self.state, TelegramRunState::Stopped) {
            return vec![i18n::run_status(RunStatus::Stopped).into()];
        }
        let mut sections: Vec<_> = self.terminal_notice().into_iter().collect();
        if !self.answer.is_empty() {
            if sections.is_empty() {
                sections.push(self.answer.clone());
            } else {
                sections.push(format!(
                    "**{}**\n\n{}",
                    i18n::PARTIAL_ANSWER_TITLE,
                    self.answer
                ));
            }
        }
        sections.push(self.status_section(draft, &sections.join("\n\n")));
        sections
    }

    fn terminal_notice(&self) -> Option<String> {
        match &self.state {
            TelegramRunState::Failed(message) => Some(Self::failure_section(message)),
            TelegramRunState::Interrupted => Some(format!(
                "**{}**\n\n{}",
                i18n::RUN_INTERRUPTED_TITLE,
                i18n::RUN_INTERRUPTED_BODY
            )),
            _ => None,
        }
    }

    fn status_section(&self, draft: bool, prefix: &str) -> String {
        if draft && !self.is_terminal() {
            format!("<tg-thinking>{}</tg-thinking>", self.draft_activity())
        } else {
            self.process_section(prefix)
        }
    }

    fn status(&self) -> RunStatus {
        match self.state {
            TelegramRunState::Queued { .. } => RunStatus::Queued,
            TelegramRunState::Running => RunStatus::Running,
            TelegramRunState::Completed => RunStatus::Completed,
            TelegramRunState::Failed(_) => RunStatus::Failed,
            TelegramRunState::Stopped => RunStatus::Stopped,
            TelegramRunState::Interrupted => RunStatus::Interrupted,
        }
    }

    fn split_sections(sections: Vec<String>) -> Vec<String> {
        let mut messages = Vec::new();
        let mut current = String::new();
        for section in sections {
            if !Self::within_limits(&section) {
                let prefix = (!current.is_empty()).then(|| std::mem::take(&mut current));
                let mut chunks = Self::safe_section_chunks(&section, prefix);
                current = chunks.pop().unwrap_or_default();
                messages.extend(chunks);
                continue;
            }

            if current.is_empty() {
                current = section;
                continue;
            }
            let candidate = format!("{current}\n\n{section}");
            if Self::within_limits(&candidate) {
                current = candidate;
            } else {
                messages.push(std::mem::replace(&mut current, section));
            }
        }
        if !current.is_empty() {
            messages.push(current);
        }
        messages
    }

    fn safe_section_chunks(section: &str, mut first_prefix: Option<String>) -> Vec<String> {
        const OPENING: &str = "<pre>";
        const CLOSING: &str = "</pre>";

        let prefix_character_cost = first_prefix
            .as_ref()
            .map_or(0, |prefix| prefix.chars().count().saturating_add(2));
        let mut character_budget = TELEGRAM_RICH_MESSAGE_MAX_CHARS
            .saturating_sub(prefix_character_cost)
            .saturating_sub(OPENING.chars().count())
            .saturating_sub(CLOSING.chars().count());
        let prefix_structure_cost = first_prefix.as_ref().map_or(0, |prefix| {
            prefix
                .lines()
                .count()
                .saturating_add(prefix.matches('<').count())
                .saturating_add(1)
        });
        let mut line_budget = TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS
            .saturating_sub(prefix_structure_cost)
            .saturating_sub(2);
        let mut chunks = Vec::new();
        let mut escaped = String::new();
        let mut character_count = 0_usize;
        let mut line_count = 1_usize;

        for character in section.chars() {
            let mut encoded = [0_u8; 4];
            let escaped_character = match character {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                _ => character.encode_utf8(&mut encoded),
            };
            let width = escaped_character.chars().count();
            let lines = usize::from(character == '\n');
            if character_count.saturating_add(width) > character_budget
                || line_count.saturating_add(lines) > line_budget
            {
                if !escaped.is_empty() {
                    Self::push_safe_chunk(&mut chunks, &mut first_prefix, &escaped);
                } else if let Some(prefix) = first_prefix.take() {
                    // The prefix fits on its own, but leaves no room for a
                    // wrapped first character. Do not exceed either budget.
                    chunks.push(prefix);
                }
                character_budget = TELEGRAM_RICH_MESSAGE_MAX_CHARS
                    .saturating_sub(OPENING.chars().count())
                    .saturating_sub(CLOSING.chars().count());
                line_budget = TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS.saturating_sub(2);
                escaped.clear();
                character_count = 0;
                line_count = 1;
            }
            escaped.push_str(escaped_character);
            character_count += width;
            line_count += lines;
        }
        if !escaped.is_empty() {
            Self::push_safe_chunk(&mut chunks, &mut first_prefix, &escaped);
        }
        chunks
    }

    fn push_safe_chunk(chunks: &mut Vec<String>, first_prefix: &mut Option<String>, escaped: &str) {
        let chunk = format!("<pre>{escaped}</pre>");
        if let Some(prefix) = first_prefix.take() {
            chunks.push(format!("{prefix}\n\n{chunk}"));
        } else {
            chunks.push(chunk);
        }
    }

    fn within_limits(rendered: &str) -> bool {
        // Each Markdown line or HTML tag can introduce a rich block. Staying below
        // this conservative combined budget leaves room under Telegram's block cap.
        rendered.chars().count() <= TELEGRAM_RICH_MESSAGE_MAX_CHARS
            && Self::structure_points(rendered) <= TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS
    }

    fn render_truncated(&self, draft: bool) -> String {
        if matches!(self.state, TelegramRunState::Stopped) {
            return i18n::TOOL_STOPPED.into();
        }
        let mut prefix = self.terminal_notice().map_or_else(String::new, |notice| {
            format!("{notice}\n\n**{}**\n\n", i18n::PARTIAL_ANSWER_TITLE)
        });
        prefix.push_str(i18n::OUTPUT_TRUNCATED);
        let status = self.status_section(draft, "");
        // An active update has a single provider message. Keep complete tags
        // and the newest answer text within both Telegram size budgets.
        let wrapper = format!("{prefix}<pre></pre>\n\n{status}");
        let char_budget = TELEGRAM_RICH_MESSAGE_MAX_CHARS.saturating_sub(wrapper.chars().count());
        let line_budget = TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS
            .saturating_sub(Self::structure_points(&wrapper) + 1);
        let tail = Self::escape_tail(&self.answer, char_budget, line_budget);
        format!("{prefix}<pre>{tail}</pre>\n\n{status}")
    }

    pub(super) fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            TelegramRunState::Completed
                | TelegramRunState::Failed(_)
                | TelegramRunState::Stopped
                | TelegramRunState::Interrupted
        )
    }

    fn apply_output(&mut self, output: OutputEvent) -> bool {
        match output {
            OutputEvent::Thinking { .. }
            | OutputEvent::Diagnostic { .. }
            | OutputEvent::Usage(_) => false,
            OutputEvent::Commentary { id, text } => self.apply_progress(
                id,
                text,
                ProgressStatus::Completed,
                TelegramProgressKind::Commentary,
                None,
            ),
            OutputEvent::Progress { id, text, status } => {
                self.apply_progress(id, text, status, TelegramProgressKind::Progress, None)
            }
            OutputEvent::ToolCall {
                id,
                text,
                status,
                web_action,
            } => {
                let title = web_action
                    .as_ref()
                    .map(i18n::web_tool_title)
                    .unwrap_or(text);
                self.apply_progress(id, title, status, TelegramProgressKind::Tool, None)
            }
            OutputEvent::CommandExecution {
                id,
                command,
                status,
                exit_code,
            } => self.apply_progress(
                id,
                command,
                status,
                TelegramProgressKind::Command,
                exit_code,
            ),
            OutputEvent::ToolOutput {
                id,
                text,
                elapsed_ms,
            } => {
                let Some(entry) = self
                    .process
                    .iter_mut()
                    .find(|e| e.id == id && e.kind.is_tool())
                else {
                    return false;
                };
                let text = if entry.kind == TelegramProgressKind::Command {
                    command_output_preview(text)
                } else {
                    text
                };
                let (text, clipped) = Self::retain_process_text(text);
                self.process_truncated |= clipped;
                let elapsed_ms =
                    elapsed_ms.or_else(|| entry.output.as_ref().and_then(|(_, ms)| *ms));
                let result = (text, elapsed_ms);
                if entry.output.as_ref() == Some(&result) {
                    return false;
                }
                entry.output = Some(result);
                true
            }
            OutputEvent::Answer { text } => {
                append_bounded_tail(
                    &mut self.answer,
                    &text,
                    TELEGRAM_RETAINED_ANSWER_MAX_BYTES,
                    i18n::OUTPUT_TRUNCATED,
                );
                !text.is_empty()
            }
            OutputEvent::Image { id, .. } => self.apply_progress(
                id,
                i18n::IMAGE_SENDING.into(),
                ProgressStatus::Running,
                TelegramProgressKind::Progress,
                None,
            ),
        }
    }

    fn apply_progress(
        &mut self,
        id: String,
        text: String,
        status: ProgressStatus,
        kind: TelegramProgressKind,
        exit_code: Option<i32>,
    ) -> bool {
        if id.len() > 512 {
            self.process_truncated = true;
            return true;
        }
        if text.trim().is_empty() && !kind.is_tool() {
            return false;
        }
        let (text, clipped) = if kind == TelegramProgressKind::Command {
            Self::retain_command(text)
        } else {
            Self::retain_process_text(text)
        };
        self.process_truncated |= clipped;
        if let Some(entry) = self.process.iter_mut().find(|entry| {
            entry.id == id
                && entry.kind == kind
                && (kind != TelegramProgressKind::Commentary || entry.text == text)
        }) {
            // A replayed start may arrive after the final status for this tool ID.
            if kind.is_tool()
                && status == ProgressStatus::Running
                && entry.status != ProgressStatus::Running
            {
                return false;
            }
            let exit_code = exit_code.or(entry.exit_code);
            if entry.text == text && entry.status == status && entry.exit_code == exit_code {
                return false;
            }
            entry.text = text;
            entry.status = status;
            entry.exit_code = exit_code;
            return true;
        }
        if self.process.len() == TELEGRAM_RETAINED_PROCESS_ENTRIES {
            self.process.pop_front();
            self.process_truncated = true;
        }
        self.process.push_back(TelegramProgressEntry {
            id,
            text,
            status,
            kind,
            exit_code,
            output: None,
        });
        true
    }

    fn retain_command(command: String) -> (String, bool) {
        // Leave room in one process block for the escaped output, title and footer.
        const MAX_ESCAPED_CHARS: usize = 16 * 1024;
        const MAX_NEWLINES: usize = 128;
        let mut chars = 0;
        let mut newlines = 0;
        for (index, ch) in command.char_indices() {
            chars += Self::escaped_width(ch);
            newlines += usize::from(ch == '\n');
            if chars > MAX_ESCAPED_CHARS || newlines > MAX_NEWLINES {
                return (
                    format!("{}{}", &command[..index], i18n::COMMAND_TRUNCATED),
                    true,
                );
            }
        }
        (command, false)
    }

    fn retain_process_text(text: String) -> (String, bool) {
        const MAX_NEWLINES: usize = 64;
        let limit = TELEGRAM_RETAINED_PROCESS_TEXT_MAX_BYTES;
        if text.len() <= limit && Self::tail_start(&text, limit, MAX_NEWLINES) == 0 {
            return (text, false);
        }

        let marker = i18n::OUTPUT_TRUNCATED;
        let text = text.strip_prefix(marker).unwrap_or(&text);
        let byte_start = text.ceil_char_boundary(text.len().saturating_sub(limit - marker.len()));
        let tail = &text[byte_start..];
        let start = Self::tail_start(
            tail,
            limit - marker.chars().count(),
            MAX_NEWLINES - marker.matches('\n').count(),
        );
        (format!("{marker}{}", &tail[start..]), true)
    }

    fn draft_activity(&self) -> String {
        if let TelegramRunState::Queued { ahead } = self.state {
            return i18n::queued_message(ahead);
        }
        match self.process.back() {
            Some(entry) if entry.kind.is_tool() && entry.status != ProgressStatus::Running => {
                i18n::run_status(self.status()).into()
            }
            Some(entry) => entry.title(),
            None => i18n::WAITING_FOR_AGENT.into(),
        }
    }

    fn process_section(&self, prefix: &str) -> String {
        let open = !self.is_terminal() && self.answer.is_empty();
        let opening = if open { "<details open>" } else { "<details>" };
        let label = match self.process.back() {
            Some(entry)
                if open
                    && matches!(self.state, TelegramRunState::Running)
                    && entry.kind.is_tool()
                    && entry.status == ProgressStatus::Running =>
            {
                entry.title()
            }
            _ => i18n::run_status(self.status()).into(),
        };
        let mut header = format!("{opening}<summary>{label}</summary>");
        let name = self.agent_name.trim();
        let agent = (!name.is_empty() && !name.eq_ignore_ascii_case("agent"))
            .then(|| Self::escape_structural_text(name));
        let mut closing = "</details>".to_owned();
        if self.process.is_empty() {
            let note = match self.state {
                TelegramRunState::Queued { ahead } => Some(i18n::queued_message(ahead)),
                TelegramRunState::Running => Some(i18n::WAITING_FOR_AGENT.into()),
                _ => None,
            };
            let text = match (agent, note) {
                (Some(agent), Some(note)) => format!("{agent}：{note}"),
                (Some(text), None) | (None, Some(text)) => text,
                (None, None) => i18n::NO_PUBLIC_STEPS.into(),
            };
            header.push_str("\n\n");
            header.push_str(&text);
        } else if let Some(agent) = agent {
            closing = format!("<footer>{agent}</footer>\n\n</details>");
        }
        // Give the answer priority. When even the process header cannot fit
        // beside it, the existing multipart sender will carry the remainder.
        let mut rows = VecDeque::new();
        let min = Self::render_process_rows(
            &header,
            &rows,
            self.process_truncated || !self.process.is_empty(),
            opening,
            &closing,
        );
        let available_chars =
            TELEGRAM_RICH_MESSAGE_MAX_CHARS.saturating_sub(prefix.chars().count() + 2);
        let available_points = TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS
            .saturating_sub(Self::structure_points(prefix) + 2);
        let (chars, points) = if min.chars().count() <= available_chars
            && Self::structure_points(&min) <= available_points
        {
            (
                available_chars.min(TELEGRAM_PROCESS_MAX_CHARS),
                available_points.min(TELEGRAM_PROCESS_MAX_STRUCTURE_POINTS),
            )
        } else {
            (
                TELEGRAM_PROCESS_MAX_CHARS,
                TELEGRAM_PROCESS_MAX_STRUCTURE_POINTS,
            )
        };
        let mut used_chars = header.chars().count();
        let mut used_points = Self::structure_points(&header);
        let mut omitted = self.process_truncated;
        for entry in self.process.iter().rev() {
            let row = self.render_entry(entry);
            used_chars += row.chars().count();
            used_points += Self::structure_points(&row);
            if used_chars > chars || used_points > points {
                omitted = true;
                break;
            }
            rows.push_front((entry.kind.is_tool(), row));
        }
        loop {
            let section = Self::render_process_rows(&header, &rows, omitted, opening, &closing);
            if rows.is_empty()
                || (section.chars().count() <= chars && Self::structure_points(&section) <= points)
            {
                return section;
            }
            rows.pop_front();
            omitted = true;
        }
    }

    fn render_process_rows(
        header: &str,
        rows: &VecDeque<(bool, String)>,
        omitted: bool,
        opening: &str,
        closing: &str,
    ) -> String {
        let mut body = vec![header.to_owned()];
        if omitted {
            body.push(i18n::PROCESS_TRUNCATED.into());
        }
        let mut rows = rows.iter().peekable();
        while let Some((is_tool, row)) = rows.next() {
            if !is_tool || !rows.peek().is_some_and(|(next_tool, _)| *next_tool) {
                body.push(row.clone());
                continue;
            }
            let mut tools = vec![row.as_str()];
            while let Some((_, row)) = rows.next_if(|(tool, _)| *tool) {
                tools.push(row);
            }
            body.push(format!(
                "{opening}<summary>{}</summary>\n\n{}\n\n</details>",
                i18n::tool_call_count(tools.len()),
                tools.join("\n\n")
            ));
        }
        body.push(closing.into());
        body.join("\n\n")
    }

    fn render_entry(&self, entry: &TelegramProgressEntry) -> String {
        let text = &entry.text;
        match entry.kind {
            TelegramProgressKind::Commentary => return Self::escape_structural_text(text),
            TelegramProgressKind::Progress => {
                let marker = match entry.status {
                    ProgressStatus::Running
                        if matches!(self.state, TelegramRunState::Interrupted) =>
                    {
                        "■"
                    }
                    ProgressStatus::Running => "●",
                    ProgressStatus::Completed => "✓",
                    ProgressStatus::Failed => "×",
                    ProgressStatus::Stopped => "■",
                };
                return format!("{marker} {}", Self::escape_structural_text(text));
            }
            _ => {}
        }
        let pending = entry.status == ProgressStatus::Running && self.is_terminal();
        let status = if pending {
            if matches!(self.state, TelegramRunState::Interrupted) {
                i18n::TOOL_STOPPED
            } else {
                i18n::TOOL_UNCONFIRMED
            }
        } else {
            match entry.status {
                ProgressStatus::Running => i18n::run_status(RunStatus::Running),
                ProgressStatus::Completed => i18n::TOOL_COMPLETED,
                ProgressStatus::Failed => i18n::TOOL_FAILED,
                ProgressStatus::Stopped => i18n::TOOL_STOPPED,
            }
        };
        let elapsed = entry.output.as_ref().and_then(|(_, ms)| *ms);
        let output = entry.output.as_ref().map_or("", |(text, _)| {
            if text.is_empty() {
                i18n::TOOL_NO_OUTPUT
            } else {
                text
            }
        });
        let detail = if entry.kind == TelegramProgressKind::Command {
            let status = if pending {
                status.to_string()
            } else {
                i18n::shell_status(entry.status, entry.exit_code)
            };
            let shell = i18n::shell_output(text, output, &status, elapsed);
            format!(
                "<pre><code class=\"language-bash\">{}</code></pre>",
                Self::escape_structural_text(&shell)
            )
        } else if entry.output.is_some() {
            let result = format!("{}{output}", i18n::tool_output_header(status, elapsed));
            format!("<pre>{}</pre>", Self::escape_structural_text(&result))
        } else {
            status.into()
        };
        format!(
            "<details><summary>{}</summary>\n\n{detail}\n\n</details>",
            entry.title()
        )
    }

    fn structure_points(text: &str) -> usize {
        text.lines().count() + text.matches('<').count()
    }

    fn failure_section(message: &str) -> String {
        let copy = i18n::failure_copy(message);
        format!(
            "**{}**\n\n{}\n\n{}",
            i18n::RUN_FAILED_TITLE,
            copy.summary,
            i18n::NATIVE_RETRY_ADVICE
        )
    }

    fn escape_structural_text(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn escape_tail(text: &str, chars: usize, newlines: usize) -> String {
        Self::escape_structural_text(&text[Self::tail_start(text, chars, newlines)..])
    }

    fn escaped_width(ch: char) -> usize {
        match ch {
            '&' => 5,
            '<' | '>' => 4,
            _ => 1,
        }
    }

    fn tail_start(text: &str, mut chars: usize, mut newlines: usize) -> usize {
        let mut start = text.len();
        for (index, character) in text.char_indices().rev() {
            let width = Self::escaped_width(character);
            if width > chars || (character == '\n' && newlines == 0) {
                break;
            }
            chars -= width;
            newlines -= usize::from(character == '\n');
            start = index;
        }
        start
    }
}

#[cfg(test)]
mod tests;
