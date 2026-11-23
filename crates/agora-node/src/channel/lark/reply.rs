use super::LarkReplyTarget;
use super::lark_api::{CotRef, LarkApi, retryable};
use crate::channel::{ChannelRun, IMAGE_DELIVERY_TIMEOUT, ImageDeliveries, RunEvent};
use crate::i18n;
use crate::task::{OutputEvent, OutputImage, ProgressStatus};
use agora_core::logger;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

mod content;
use content::Content;

#[cfg(test)]
mod tests;

const UPDATE_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Clone)]
pub(super) struct LarkAgentReply {
    inner: Arc<Inner>,
}

struct Inner {
    target: LarkReplyTarget,
    chat_id: String,
    api: LarkApi,
    state: Mutex<State>,
    flush: Mutex<()>,
}

struct State {
    content: Content,
    images: ImageDeliveries,
    cot: Option<CotRef>,
    cot_creation_attempted: bool,
    cot_completed: bool,
    answer_id: Option<String>,
    answer_uuid: String,
    // Freeze the initial body until creation is confirmed, including after an
    // ambiguous response. Never reuse a reply uuid with different content.
    initial_answer: Option<String>,
    sent_answer: String,
    version: u64,
    sent_version: u64,
    last_update: Option<Instant>,
    flush_scheduled: bool,
    terminal_retry_started: bool,
}

impl LarkAgentReply {
    pub(super) fn new(
        target: LarkReplyTarget,
        chat_id: String,
        agent: String,
        api: LarkApi,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    content: Content::new(agent, target.message_id.clone()),
                    images: ImageDeliveries::default(),
                    cot: None,
                    cot_creation_attempted: false,
                    cot_completed: false,
                    answer_id: None,
                    answer_uuid: uuid::Uuid::new_v4().to_string(),
                    initial_answer: None,
                    sent_answer: String::new(),
                    version: 0,
                    sent_version: 0,
                    last_update: None,
                    flush_scheduled: false,
                    terminal_retry_started: false,
                }),
                target,
                chat_id,
                api,
                flush: Mutex::new(()),
            }),
        }
    }

    async fn publish_event(&self, event: RunEvent) -> Result<()> {
        let flush_now = {
            let mut state = self.inner.state.lock().await;
            if state.content.terminal.is_some() {
                drop(state);
                return self.flush_latest().await;
            }
            let immediate = !matches!(event, RunEvent::Output(_));
            if !state.content.apply(event) {
                return Ok(());
            }
            state.version = state.version.saturating_add(1);
            if !immediate && !state.flush_scheduled {
                state.flush_scheduled = true;
                let delay = state
                    .last_update
                    .map(|last| UPDATE_INTERVAL.saturating_sub(last.elapsed()))
                    .unwrap_or_default();
                self.schedule_flush(delay);
            }
            immediate
        };
        if flush_now {
            self.flush_latest().await
        } else {
            Ok(())
        }
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
                let reply = Self { inner };
                let version = reply.inner.state.lock().await.version;
                if let Err(error) = reply.flush_latest().await {
                    logger::error!(
                        "lark native progress update failed source_message_id={} error={}",
                        reply.inner.target.message_id,
                        error
                    );
                }
                let mut state = reply.inner.state.lock().await;
                if state.version == state.sent_version
                    || state.version == version
                    || state.content.terminal.is_some()
                {
                    state.flush_scheduled = false;
                    return;
                }
                delay = UPDATE_INTERVAL;
            }
        });
    }

    async fn flush_process(&self) -> Result<()> {
        let cot = {
            let mut state = self.inner.state.lock().await;
            if let Some(cot) = &state.cot {
                cot.clone()
            } else if state.cot_creation_attempted {
                // A create cannot be repeated after an uncertain response or cancellation.
                state.content.pending.clear();
                return Ok(());
            } else {
                state.cot_creation_attempted = true;
                drop(state);
                match self
                    .inner
                    .api
                    .create_cot(&self.inner.chat_id, &self.inner.target.message_id)
                    .await
                {
                    Ok(cot) => {
                        self.inner.state.lock().await.cot = Some(cot.clone());
                        cot
                    }
                    Err(error) => {
                        logger::error!(
                            "lark process creation unconfirmed, using quoted reply source_message_id={} error={}",
                            self.inner.target.message_id,
                            error
                        );
                        return Ok(());
                    }
                }
            }
        };
        loop {
            let events = self
                .inner
                .state
                .lock()
                .await
                .content
                .pending
                .iter()
                .take(50)
                .cloned()
                .collect::<Vec<_>>();
            if events.is_empty() {
                break;
            }
            self.inner.api.append_cot(&cot, &events).await?;
            self.inner
                .state
                .lock()
                .await
                .content
                .pending
                .drain(..events.len());
        }
        let complete = {
            let state = self.inner.state.lock().await;
            (state.content.terminal.is_some()
                && state.content.pending.is_empty()
                && !state.cot_completed)
                .then(|| state.content.success())
        };
        if let Some(success) = complete {
            self.inner.api.complete_cot(&cot, success).await?;
            self.inner.state.lock().await.cot_completed = true;
        }
        Ok(())
    }

    // Caller holds the publication lock. Send once at the terminal state, or
    // earlier before an image upload if answer text is already available.
    // Only the terminal state may edit an early answer (native posts allow at
    // most 20 edits); process deltas never create additional answer messages.
    async fn flush_answer(&self) -> Result<()> {
        let (id, text, initial, uuid, terminal) = {
            let mut state = self.inner.state.lock().await;
            let terminal = state.content.terminal.is_some();
            let mut text = state.content.final_text();
            if text.is_empty() && state.cot_creation_attempted && state.cot.is_none() {
                text = if terminal {
                    i18n::run_status(i18n::RunStatus::Stopped).into()
                } else {
                    i18n::run_status(i18n::RunStatus::Queued).into()
                };
            }
            if text.is_empty()
                || (state.answer_id.is_some() && (!terminal || text == state.sent_answer))
            {
                return Ok(());
            }
            if state.answer_id.is_none() && state.initial_answer.is_none() {
                state.initial_answer = Some(text.clone());
            }
            (
                state.answer_id.clone(),
                text,
                state.initial_answer.clone(),
                state.answer_uuid.clone(),
                terminal,
            )
        };
        let id = if let Some(id) = id {
            id
        } else {
            let initial = initial.unwrap();
            let id = self
                .inner
                .api
                .reply_post(&self.inner.target, &initial, &uuid)
                .await?;
            let mut state = self.inner.state.lock().await;
            state.answer_id = Some(id.clone());
            state.sent_answer = initial;
            id
        };
        let needs_update = terminal && self.inner.state.lock().await.sent_answer != text;
        if needs_update {
            self.inner
                .api
                .update_post(&id, &text, &self.inner.target)
                .await?;
            self.inner.state.lock().await.sent_answer = text;
        }
        Ok(())
    }

    async fn flush_latest(&self) -> Result<()> {
        let _flush = self.inner.flush.lock().await;
        let version = {
            let state = self.inner.state.lock().await;
            if state.version == state.sent_version {
                return Ok(());
            }
            state.version
        };
        let process_result = self.flush_process().await;
        // A process API failure must not suppress an available final answer.
        let needs_answer = {
            let state = self.inner.state.lock().await;
            state.content.terminal.is_some()
                || (state.cot_creation_attempted && state.cot.is_none())
        };
        let answer_result = if needs_answer {
            self.flush_answer().await
        } else {
            Ok(())
        };
        let mut state = self.inner.state.lock().await;
        state.last_update = Some(Instant::now());
        if process_result.is_ok() && answer_result.is_ok() {
            state.sent_version = version;
        }
        answer_result.and(process_result)
    }

    async fn publish_image(&self, id: String, image: OutputImage) -> Result<()> {
        let admitted = {
            let mut state = self.inner.state.lock().await;
            if state.content.terminal.is_some() {
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
                {
                    let _flush = self.inner.flush.lock().await;
                    if let Err(error) = self.flush_answer().await {
                        logger::error!("lark answer before image not confirmed: {}", error);
                    }
                }
                tokio::time::timeout(
                    IMAGE_DELIVERY_TIMEOUT,
                    self.inner.api.reply_image(&self.inner.target, &image),
                )
                .await
                .context("lark image delivery timed out")
                .and_then(|result| result)
            }
            Err(error) => Err(error),
        };
        let (text, status) = match result {
            Ok(()) => (i18n::IMAGE_SENT, ProgressStatus::Completed),
            Err(error) => {
                logger::error!("lark image delivery not confirmed: {}", error);
                (i18n::IMAGE_SEND_FAILED, ProgressStatus::Failed)
            }
        };
        self.publish_event(RunEvent::Output(OutputEvent::Progress {
            id,
            text: text.into(),
            status,
        }))
        .await
    }

    async fn retry_terminal(&self, error: anyhow::Error) -> Result<()> {
        if !retryable(&error) {
            return Err(error);
        }
        {
            let mut state = self.inner.state.lock().await;
            if !state.cot_creation_attempted
                || state.content.terminal.is_none()
                || state.terminal_retry_started
            {
                return Err(error);
            }
            if state.version == state.sent_version {
                return Ok(());
            }
            state.terminal_retry_started = true;
        }
        // The run's completion guard stays alive until delivery settles. Normal
        // shutdown can therefore use its existing bounded wait for this work.
        let retry = async {
            let mut last_error = error;
            for delay in [1, 2, 4] {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                match self.flush_latest().await {
                    Ok(()) => return Ok(()),
                    Err(error) if !retryable(&error) => return Err(error),
                    Err(error) => last_error = error,
                }
            }
            Err(last_error)
        };
        let result = tokio::time::timeout(Duration::from_secs(60), retry)
            .await
            .context("lark terminal delivery retry timed out")
            .and_then(|r| r);
        if result.is_err() {
            logger::error!(
                "lark terminal delivery exhausted source_message_id={} terminal_publication_failed=true",
                self.inner.target.message_id
            );
        }
        result
    }
}

impl ChannelRun for LarkAgentReply {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        let result = if let RunEvent::Output(OutputEvent::Image { id, image }) = event {
            self.publish_image(id, image).await
        } else {
            self.publish_event(event).await
        };
        match result {
            Err(error) => self.retry_terminal(error).await,
            ok => ok,
        }
    }
}
