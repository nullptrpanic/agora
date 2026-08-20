use super::channel::{TelegramInterruptRegistration, TelegramReplyTarget};
use super::telegram_api::TelegramApi;
use crate::channel::{ChannelRun, RunEvent, append_bounded_tail, bounded_tail};
use crate::i18n::{self, RunStatus};
use crate::task::{OutputEvent, ProgressStatus, TokenUsage};
use agora_core::logger;
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TELEGRAM_UPDATE_INTERVAL: Duration = Duration::from_millis(400);
const TELEGRAM_DRAFT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const TELEGRAM_DELIVERY_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const TELEGRAM_DELIVERY_MAX_FAILURES: u32 = 3;
const TELEGRAM_RICH_MESSAGE_MAX_CHARS: usize = 32_768;
const TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS: usize = 400;
const TELEGRAM_PROCESS_MAX_CHARS: usize = 20_000;
const TELEGRAM_PROCESS_MAX_STRUCTURE_POINTS: usize = 240;
const TELEGRAM_RETAINED_ANSWER_MAX_BYTES: usize = 256 * 1024;
const TELEGRAM_RETAINED_PROCESS_PHASES: usize = 64;
const TELEGRAM_RETAINED_PROGRESS_PER_PHASE: usize = 64;
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
    content: TelegramRichContent,
    draft_id: i64,
    message_ids: Vec<i64>,
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
                    content: TelegramRichContent::new(agent_name),
                    draft_id,
                    message_ids: Vec::new(),
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
                state.content.apply(event);
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
            tokio::time::sleep(delay).await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let message = TelegramRichMessage { inner };
            {
                let mut state = message.inner.state.lock().await;
                state.flush_scheduled = false;
            }
            if let Err(err) = message.flush_latest(false).await {
                message.handle_flush_failure("update", &err).await;
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
                messages: state.content.render_messages(draft),
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
                let message_id = self
                    .inner
                    .api
                    .send_rich_message(&self.inner.target, primary, callback_data.as_deref())
                    .await?;
                self.remember_message_id(0, message_id).await;
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
                let message_id = self
                    .inner
                    .api
                    .send_rich_message(&self.inner.target, markdown, None)
                    .await?;
                self.remember_message_id(index, message_id).await;
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

    async fn remember_message_id(&self, index: usize, message_id: i64) {
        let mut state = self.inner.state.lock().await;
        if let Some(existing) = state.message_ids.get_mut(index) {
            *existing = message_id;
        } else {
            debug_assert_eq!(state.message_ids.len(), index);
            state.message_ids.push(message_id);
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
            if !pending || !retryable {
                None
            } else if !state.retry_scheduled {
                state.delivery_failures = state.delivery_failures.saturating_add(1);
                if state.delivery_failures > TELEGRAM_DELIVERY_MAX_FAILURES {
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
            "telegram rich message {} failed chat_id={} retry_scheduled={} error={}",
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
            {
                let mut state = message.inner.state.lock().await;
                state.retry_scheduled = false;
            }
            if let Err(err) = message.flush_latest(false).await {
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
        self.publish_event(event).await
    }
}

pub(super) struct TelegramRichContent {
    agent_name: String,
    process: VecDeque<TelegramProcessPhase>,
    process_truncated: bool,
    answer: String,
    usage: Option<TokenUsage>,
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
}

struct TelegramProcessPhase {
    thinking: Option<String>,
    progress: VecDeque<TelegramProgressEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TelegramProgressKind {
    Message,
    Command,
}

impl TelegramRichContent {
    pub(super) fn new(agent_name: String) -> Self {
        Self {
            agent_name,
            process: VecDeque::new(),
            process_truncated: false,
            answer: String::new(),
            usage: None,
            state: TelegramRunState::Running,
        }
    }

    pub(super) fn apply(&mut self, event: RunEvent) {
        if self.is_terminal() {
            return;
        }
        match event {
            RunEvent::Queued { ahead } => self.state = TelegramRunState::Queued { ahead },
            RunEvent::Started { .. } => self.state = TelegramRunState::Running,
            RunEvent::Output(output) => self.apply_output(output),
            RunEvent::Completed { .. } => self.state = TelegramRunState::Completed,
            RunEvent::Failed { message } => self.state = TelegramRunState::Failed(message),
            RunEvent::Stopped => {
                self.stop_running_progress();
                self.state = TelegramRunState::Stopped;
            }
            RunEvent::Interrupted => {
                self.stop_running_progress();
                self.state = TelegramRunState::Interrupted;
            }
        }
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

    pub(super) fn render_messages(&self, draft: bool) -> Vec<String> {
        let sections = self.render_sections(draft);
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
        let mut sections = Vec::new();
        if !draft || self.is_terminal() {
            sections.push(self.header_section());
        }
        if let Some(section) = self.terminal_state_section() {
            sections.push(section);
        }
        if let Some(section) = self.active_state_section(draft) {
            sections.push(section);
        }

        if self.is_terminal()
            && let Some(section) = self.answer_section()
        {
            sections.push(section);
        }
        if (!draft || self.is_terminal())
            && let Some(section) = self.process_section()
        {
            sections.push(section);
        }
        if !self.is_terminal()
            && let Some(section) = self.answer_section()
        {
            sections.push(section);
        }
        if self.is_terminal()
            && let Some(usage) = self.usage
        {
            sections.push(Self::usage_section(usage));
        }
        sections
    }

    fn header_section(&self) -> String {
        let status = match self.state {
            TelegramRunState::Queued { .. } => RunStatus::Queued,
            TelegramRunState::Running => RunStatus::Running,
            TelegramRunState::Completed => RunStatus::Completed,
            TelegramRunState::Failed(_) => RunStatus::Failed,
            TelegramRunState::Stopped => RunStatus::Stopped,
            TelegramRunState::Interrupted => RunStatus::Interrupted,
        };
        format!(
            "## {}\n\n> **{}**",
            Self::escape_structural_text(&self.agent_name),
            i18n::run_status(status)
        )
    }

    fn answer_section(&self) -> Option<String> {
        if self.answer.is_empty() {
            return None;
        }
        let title = if self.has_partial_answer() {
            Some(i18n::PARTIAL_ANSWER_TITLE)
        } else if matches!(self.state, TelegramRunState::Completed) {
            Some(i18n::FINAL_ANSWER_TITLE)
        } else {
            None
        };
        Some(title.map_or_else(
            || self.answer.clone(),
            |title| format!("### {title}\n\n{}", self.answer),
        ))
    }

    fn split_sections(sections: Vec<String>) -> Vec<String> {
        let mut messages = Vec::new();
        let mut current = String::new();
        for section in sections {
            if !Self::within_limits(&section) {
                let prefix = (!current.is_empty()).then(|| std::mem::take(&mut current));
                messages.extend(Self::safe_section_chunks(&section, prefix));
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
            if !escaped.is_empty()
                && (character_count.saturating_add(width) > character_budget
                    || line_count.saturating_add(lines) > line_budget)
            {
                Self::push_safe_chunk(&mut chunks, &mut first_prefix, &escaped);
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
            && rendered
                .lines()
                .count()
                .saturating_add(rendered.matches('<').count())
                <= TELEGRAM_RICH_MESSAGE_MAX_STRUCTURE_POINTS
    }

    fn render_truncated(&self, draft: bool) -> String {
        let mut sections = Vec::new();
        if !draft || self.is_terminal() {
            sections.push(self.header_section());
        }
        if let Some(section) = self.active_state_section(draft) {
            sections.push(section);
        }
        if let Some(section) = self.terminal_state_section() {
            sections.push(section);
        }
        sections.push(format!("> {}", i18n::OUTPUT_TRUNCATED.trim()));
        let usage = if self.is_terminal() {
            self.usage.map(Self::usage_section)
        } else {
            None
        };

        if self.answer.is_empty() {
            if let Some(usage) = usage {
                sections.push(usage);
            }
            return sections.join("\n\n");
        }

        let answer_heading = if self.has_partial_answer() {
            format!("### {}\n\n", i18n::PARTIAL_ANSWER_TITLE)
        } else if matches!(self.state, TelegramRunState::Completed) {
            format!("### {}\n\n", i18n::FINAL_ANSWER_TITLE)
        } else {
            String::new()
        };
        let prefix = format!("{}\n\n{answer_heading}<pre>", sections.join("\n\n"));
        let closing = usage
            .map(|usage| format!("</pre>\n\n{usage}"))
            .unwrap_or_else(|| "</pre>".to_string());
        let answer_budget = TELEGRAM_RICH_MESSAGE_MAX_CHARS
            .saturating_sub(prefix.chars().count())
            .saturating_sub(closing.chars().count());
        let answer = Self::escape_tail(&self.answer, answer_budget);
        format!("{prefix}{answer}{closing}")
    }

    fn active_state_section(&self, draft: bool) -> Option<String> {
        match &self.state {
            TelegramRunState::Queued { ahead } => {
                Some(format!("> {}", i18n::queued_message(*ahead)))
            }
            TelegramRunState::Running => {
                if draft {
                    let activity = self.draft_activity();
                    Some(format!("<tg-thinking>{activity}</tg-thinking>"))
                } else if !self.process.is_empty() {
                    None
                } else {
                    Some(format!(
                        "> {}",
                        Self::escape_structural_text(i18n::WAITING_FOR_AGENT)
                    ))
                }
            }
            TelegramRunState::Completed
            | TelegramRunState::Failed(_)
            | TelegramRunState::Stopped
            | TelegramRunState::Interrupted => None,
        }
    }

    fn terminal_state_section(&self) -> Option<String> {
        match &self.state {
            TelegramRunState::Failed(message) => Some(Self::failure_section(message)),
            TelegramRunState::Stopped => Some(format!(
                "**{}**\n\n{}",
                i18n::RUN_STOPPED_TITLE,
                i18n::RUN_STOPPED_BODY
            )),
            TelegramRunState::Interrupted => Some(format!(
                "**{}**\n\n{}",
                i18n::RUN_INTERRUPTED_TITLE,
                i18n::RUN_INTERRUPTED_BODY
            )),
            TelegramRunState::Queued { .. }
            | TelegramRunState::Running
            | TelegramRunState::Completed => None,
        }
    }

    fn has_partial_answer(&self) -> bool {
        matches!(
            self.state,
            TelegramRunState::Failed(_) | TelegramRunState::Stopped | TelegramRunState::Interrupted
        )
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

    fn apply_output(&mut self, output: OutputEvent) {
        match output {
            OutputEvent::Thinking { text } => {
                if !text.trim().is_empty() {
                    let (text, truncated) = bounded_tail(
                        text,
                        TELEGRAM_RETAINED_PROCESS_TEXT_MAX_BYTES,
                        i18n::OUTPUT_TRUNCATED,
                    );
                    self.process_truncated |= truncated;
                    if self.process.len() >= TELEGRAM_RETAINED_PROCESS_PHASES {
                        self.process.pop_back();
                        self.process_truncated = true;
                    }
                    self.process.push_front(TelegramProcessPhase {
                        thinking: Some(text),
                        progress: VecDeque::new(),
                    });
                }
            }
            OutputEvent::Progress { id, text, status } => {
                self.apply_progress(id, text, status, TelegramProgressKind::Message, None);
            }
            OutputEvent::CommandExecution {
                id,
                command,
                status,
                exit_code,
            } => {
                self.apply_progress(
                    id,
                    command,
                    status,
                    TelegramProgressKind::Command,
                    exit_code,
                );
            }
            OutputEvent::Answer { text } => {
                append_bounded_tail(
                    &mut self.answer,
                    &text,
                    TELEGRAM_RETAINED_ANSWER_MAX_BYTES,
                    i18n::OUTPUT_TRUNCATED,
                );
            }
            OutputEvent::Usage(usage) => self.usage = Some(usage),
        }
    }

    fn apply_progress(
        &mut self,
        id: String,
        text: String,
        status: ProgressStatus,
        kind: TelegramProgressKind,
        exit_code: Option<i32>,
    ) {
        let (text, truncated) = bounded_tail(
            text,
            TELEGRAM_RETAINED_PROCESS_TEXT_MAX_BYTES,
            i18n::OUTPUT_TRUNCATED,
        );
        self.process_truncated |= truncated;
        for phase in &mut self.process {
            if let Some(entry) = phase.progress.iter_mut().find(|entry| entry.id == id) {
                entry.text = text;
                entry.status = status;
                entry.kind = kind;
                entry.exit_code = exit_code;
                return;
            }
        }
        if self.process.is_empty() {
            self.process.push_front(TelegramProcessPhase {
                thinking: None,
                progress: VecDeque::new(),
            });
        }
        let phase = self.process.front_mut().expect("process phase must exist");
        if phase.progress.len() >= TELEGRAM_RETAINED_PROGRESS_PER_PHASE {
            phase.progress.pop_front();
            self.process_truncated = true;
        }
        phase.progress.push_back(TelegramProgressEntry {
            id,
            text,
            status,
            kind,
            exit_code,
        });
    }

    fn stop_running_progress(&mut self) {
        for entry in self
            .process
            .iter_mut()
            .flat_map(|phase| phase.progress.iter_mut())
        {
            if entry.status == ProgressStatus::Running {
                entry.status = ProgressStatus::Stopped;
            }
        }
    }

    fn progress_marker(status: ProgressStatus) -> &'static str {
        match status {
            ProgressStatus::Running => "●",
            ProgressStatus::Completed => "✓",
            ProgressStatus::Failed => "×",
            ProgressStatus::Stopped => "■",
        }
    }

    fn draft_activity(&self) -> String {
        let Some(phase) = self.process.front() else {
            return Self::escape_structural_text(i18n::WAITING_FOR_AGENT);
        };
        let mut activity = phase
            .thinking
            .as_deref()
            .map(Self::escape_structural_text)
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(progress) = phase.progress.back() {
            let prefix = if progress.kind == TelegramProgressKind::Command {
                "$ "
            } else {
                ""
            };
            activity.push(format!(
                "{} {prefix}{}",
                Self::progress_marker(progress.status),
                Self::escape_structural_text(&progress.text)
            ));
        }
        if activity.is_empty() {
            Self::escape_structural_text(i18n::WAITING_FOR_AGENT)
        } else {
            activity.join("\n\n")
        }
    }

    fn process_section(&self) -> Option<String> {
        if self.process.is_empty() {
            return None;
        }
        let mut phases = self
            .process
            .iter()
            .rev()
            .enumerate()
            .map(|(index, phase)| Self::phase_section(phase, index + 1))
            .collect::<VecDeque<_>>();
        let mut omitted = 0;
        loop {
            let section = self.render_process_section(&phases, omitted);
            if Self::within_process_limits(&section) || phases.len() <= 1 {
                return Some(section);
            }
            phases.pop_front();
            omitted += 1;
        }
    }

    fn render_process_section(&self, phases: &VecDeque<String>, omitted: usize) -> String {
        let opening = if self.is_terminal() {
            "<details>"
        } else {
            "<details open>"
        };
        let mut summary = format!(
            "{} · {}",
            i18n::PROCESS_TITLE,
            i18n::phase_count(self.process.len())
        );
        let progress = self.progress_summary();
        if !progress.is_empty() {
            summary.push_str(" · ");
            summary.push_str(&progress);
        }

        let mut body = Vec::new();
        if self.process_truncated {
            body.push(format!("> {}", i18n::OUTPUT_TRUNCATED.trim()));
        }
        if omitted > 0 {
            body.push(format!("> {}", i18n::truncated_phase_count(omitted)));
        }
        body.extend(phases.iter().cloned());
        format!(
            "{opening}<summary>{summary}</summary>\n\n{}\n\n</details>",
            body.join("\n\n---\n\n")
        )
    }

    fn within_process_limits(rendered: &str) -> bool {
        rendered.chars().count() <= TELEGRAM_PROCESS_MAX_CHARS
            && rendered
                .lines()
                .count()
                .saturating_add(rendered.matches('<').count())
                <= TELEGRAM_PROCESS_MAX_STRUCTURE_POINTS
    }

    fn phase_section(phase: &TelegramProcessPhase, phase_number: usize) -> String {
        let mut sections = Vec::new();
        if let Some(thinking) = phase.thinking.as_deref() {
            let thinking = Self::escape_structural_text(thinking)
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    if index == 0 {
                        format!("> ✦ {line}")
                    } else {
                        format!("> {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!(
                "**{phase_number:02} · {}**\n\n{thinking}",
                i18n::THINKING_TITLE
            ));
        }

        sections.extend(
            phase
                .progress
                .iter()
                .filter(|entry| entry.kind == TelegramProgressKind::Command)
                .map(Self::terminal_section),
        );

        let progress = phase
            .progress
            .iter()
            .filter(|entry| entry.kind == TelegramProgressKind::Message)
            .map(|entry| {
                format!(
                    "- {} {}",
                    Self::progress_marker(entry.status),
                    Self::escape_structural_text(&entry.text)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !progress.is_empty() {
            sections.push(progress);
        }
        sections.join("\n\n")
    }

    fn terminal_section(entry: &TelegramProgressEntry) -> String {
        let status = entry.exit_code.map_or_else(
            || Self::progress_status_label(entry.status).to_string(),
            |exit_code| format!("exit {exit_code}"),
        );
        format!(
            "<pre><code class=\"language-bash\"># {} · {} {status}\n$ {}</code></pre>",
            i18n::SHELL_TITLE,
            Self::progress_marker(entry.status),
            Self::escape_structural_text(&entry.text)
        )
    }

    fn progress_status_label(status: ProgressStatus) -> &'static str {
        match status {
            ProgressStatus::Running => i18n::SHELL_RUNNING,
            ProgressStatus::Completed => i18n::SHELL_COMPLETED,
            ProgressStatus::Failed => i18n::SHELL_FAILED,
            ProgressStatus::Stopped => i18n::SHELL_STOPPED,
        }
    }

    fn progress_summary(&self) -> String {
        let mut counts = [0_usize; 4];
        for entry in self.progress_entries() {
            let index = match entry.status {
                ProgressStatus::Running => 0,
                ProgressStatus::Completed => 1,
                ProgressStatus::Failed => 2,
                ProgressStatus::Stopped => 3,
            };
            counts[index] += 1;
        }
        [
            (ProgressStatus::Completed, "✓", counts[1]),
            (ProgressStatus::Running, "●", counts[0]),
            (ProgressStatus::Failed, "×", counts[2]),
            (ProgressStatus::Stopped, "■", counts[3]),
        ]
        .into_iter()
        .filter(|(_, _, count)| *count > 0)
        .map(|(status, marker, count)| format!("{marker} {}", i18n::progress_count(status, count)))
        .collect::<Vec<_>>()
        .join(" · ")
    }

    fn progress_entries(&self) -> impl Iterator<Item = &TelegramProgressEntry> {
        self.process.iter().flat_map(|phase| phase.progress.iter())
    }

    fn usage_section(usage: TokenUsage) -> String {
        let total = usage.input_tokens.saturating_add(usage.output_tokens);
        format!(
            "*{} {} · {} {} · {} · {} {} · {} {}*",
            Self::format_tokens(total),
            i18n::TOKENS,
            i18n::INPUT,
            Self::format_tokens(usage.input_tokens),
            i18n::cached_tokens(Self::format_tokens(usage.cached_input_tokens)),
            i18n::OUTPUT,
            Self::format_tokens(usage.output_tokens),
            i18n::REASONING,
            Self::format_tokens(usage.reasoning_output_tokens)
        )
    }

    fn format_tokens(tokens: u64) -> String {
        if tokens < 1_000 {
            tokens.to_string()
        } else if tokens < 1_000_000 {
            format!("{:.1}K", tokens as f64 / 1_000.0)
        } else {
            format!("{:.1}M", tokens as f64 / 1_000_000.0)
        }
    }

    fn failure_section(message: &str) -> String {
        let copy = i18n::failure_copy(message);
        format!(
            "**{}**\n\n{}\n\n{}",
            i18n::RUN_FAILED_TITLE,
            copy.summary,
            i18n::RETRY_ADVICE
        )
    }

    fn escape_structural_text(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn escape_tail(text: &str, budget: usize) -> String {
        let mut start = text.len();
        let mut escaped_chars = 0_usize;
        for (index, character) in text.char_indices().rev() {
            let width = match character {
                '&' => "&amp;".chars().count(),
                '<' => "&lt;".chars().count(),
                '>' => "&gt;".chars().count(),
                _ => 1,
            };
            if escaped_chars.saturating_add(width) > budget {
                break;
            }
            escaped_chars += width;
            start = index;
        }
        Self::escape_structural_text(&text[start..])
    }
}

#[cfg(test)]
mod tests;
