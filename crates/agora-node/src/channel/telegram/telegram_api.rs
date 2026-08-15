use super::channel::TelegramReplyTarget;
use crate::config::TelegramChannelConfig;
use crate::http;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use reqwest::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const TELEGRAM_BOT_API: &str = "https://api.telegram.org";
const TELEGRAM_LONG_POLL_SECONDS: u64 = 50;
const TELEGRAM_HTTP_MAX_IDLE_CONNECTIONS_PER_HOST: usize = 10;
const TELEGRAM_HTTP_IDLE_TIMEOUT_SECONDS: u64 = 300;
const TELEGRAM_HTTP_CONNECT_TIMEOUT_SECONDS: u64 = 10;
const TELEGRAM_HTTP_REQUEST_TIMEOUT_SECONDS: u64 = 60;
const TELEGRAM_REQUEST_MAX_ATTEMPTS: usize = 3;
const TELEGRAM_REQUEST_RETRY_DELAY_MILLIS: u64 = 250;

#[derive(Clone)]
pub(super) struct TelegramApi {
    name: String,
    token: String,
    client: Client,
    base_url: String,
    next_draft_id: Arc<AtomicI64>,
}

impl TelegramApi {
    pub(super) fn new(config: TelegramChannelConfig) -> Result<Self> {
        Self::with_base_url(config, TELEGRAM_BOT_API.to_string())
    }

    pub(super) fn with_base_url(config: TelegramChannelConfig, base_url: String) -> Result<Self> {
        let client = Self::http_client(config.proxy.as_ref())?;
        Ok(Self {
            name: config.name,
            token: config.token,
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            next_draft_id: Arc::new(AtomicI64::new(Self::draft_id_seed())),
        })
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) async fn bot_username(&self) -> Result<String> {
        let user: TelegramUser = self.request("getMe", &EmptyRequest {}).await?;
        user.username
            .filter(|username| !username.is_empty())
            .ok_or_else(|| anyhow!("telegram getMe response missing bot username"))
    }

    pub(super) async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<Value>> {
        self.request(
            "getUpdates",
            &GetUpdatesRequest {
                offset,
                timeout: TELEGRAM_LONG_POLL_SECONDS,
                allowed_updates: ["message", "callback_query"],
            },
        )
        .await
    }

    pub(super) async fn answer_callback_query(&self, query_id: &str) -> Result<()> {
        let answered: bool = self
            .request(
                "answerCallbackQuery",
                &AnswerCallbackQueryRequest {
                    callback_query_id: query_id,
                },
            )
            .await?;
        if !answered {
            bail!("telegram answerCallbackQuery returned false");
        }
        Ok(())
    }

    pub(super) async fn set_commands(&self, commands: &[TelegramBotCommand<'_>]) -> Result<()> {
        let updated: bool = self
            .request("setMyCommands", &SetMyCommandsRequest { commands })
            .await?;
        if !updated {
            bail!("telegram setMyCommands returned false");
        }
        Ok(())
    }

    pub(super) async fn download_file(
        &self,
        file_id: &str,
        maximum_bytes: usize,
    ) -> std::result::Result<TelegramFileResource, TelegramFileDownloadError> {
        let file: TelegramFile = self
            .request_with_attempts(
                "getFile",
                &GetFileRequest { file_id },
                TELEGRAM_REQUEST_MAX_ATTEMPTS,
            )
            .await
            .map_err(TelegramFileDownloadError::from_api)?;
        let file_path = file
            .file_path
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                TelegramFileDownloadError::permanent("telegram getFile response missing file path")
            })?;
        let response = self
            .client
            .get(format!(
                "{}/file/bot{}/{}",
                self.base_url,
                self.token,
                file_path.trim_start_matches('/')
            ))
            .send()
            .await
            .map_err(|err| {
                TelegramFileDownloadError::from_api(Self::safe_transport_error(
                    "downloadFile",
                    "request",
                    &err,
                    true,
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(TelegramFileDownloadError::new(
                status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
                format!("telegram downloadFile failed http status={status}"),
            ));
        }
        let media_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .unwrap_or("application/octet-stream")
            .to_string();
        let data = http::read_body_limited(response, maximum_bytes)
            .await
            .map_err(|err| {
                TelegramFileDownloadError::new(
                    !err.is_limit_exceeded(),
                    format!("telegram downloadFile response failed: {err}"),
                )
            })?;
        let file_name = file_path
            .rsplit('/')
            .find(|name| !name.is_empty())
            .unwrap_or("telegram-image.jpg")
            .to_string();
        Ok(TelegramFileResource {
            file_name,
            media_type,
            data,
        })
    }

    pub(super) fn allocate_draft_id(&self) -> i64 {
        let draft_id = self.next_draft_id.fetch_add(1, Ordering::Relaxed);
        if draft_id == 0 {
            self.next_draft_id.fetch_add(1, Ordering::Relaxed)
        } else {
            draft_id
        }
    }

    pub(super) async fn send_rich_message_draft(
        &self,
        target: &TelegramReplyTarget,
        draft_id: i64,
        markdown: &str,
    ) -> Result<()> {
        let sent: bool = self
            .request(
                "sendRichMessageDraft",
                &SendRichMessageDraftRequest {
                    chat_id: target.chat_id,
                    message_thread_id: target.message_thread_id,
                    draft_id,
                    rich_message: InputRichMessage { markdown },
                },
            )
            .await?;
        if !sent {
            bail!("telegram sendRichMessageDraft returned false");
        }
        Ok(())
    }

    pub(super) async fn send_rich_message(
        &self,
        target: &TelegramReplyTarget,
        markdown: &str,
        callback_data: Option<&str>,
    ) -> Result<i64> {
        let message: TelegramSentMessage = self
            .request_once(
                "sendRichMessage",
                &SendRichMessageRequest {
                    chat_id: target.chat_id,
                    message_thread_id: target.message_thread_id,
                    rich_message: InputRichMessage { markdown },
                    reply_parameters: ReplyParameters {
                        message_id: target.message_id,
                    },
                    reply_markup: callback_data.map(TelegramInlineKeyboardMarkup::stop),
                },
            )
            .await?;
        Ok(message.message_id)
    }

    pub(super) async fn edit_rich_message(
        &self,
        chat_id: i64,
        message_id: i64,
        markdown: &str,
        callback_data: Option<&str>,
    ) -> Result<()> {
        let _: TelegramSentMessage = self
            .request(
                "editMessageText",
                &EditRichMessageRequest {
                    chat_id,
                    message_id,
                    rich_message: InputRichMessage { markdown },
                    reply_markup: TelegramInlineKeyboardMarkup::stop_or_empty(callback_data),
                },
            )
            .await?;
        Ok(())
    }

    async fn request<B, T>(&self, method: &str, body: &B) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        self.request_with_attempts(method, body, TELEGRAM_REQUEST_MAX_ATTEMPTS)
            .await
            .map_err(anyhow::Error::new)
    }

    async fn request_once<B, T>(&self, method: &str, body: &B) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        self.request_with_attempts(method, body, 1)
            .await
            .map_err(anyhow::Error::new)
    }

    async fn request_with_attempts<B, T>(
        &self,
        method: &str,
        body: &B,
        max_attempts: usize,
    ) -> std::result::Result<T, TelegramApiError>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let mut attempt = 1;
        loop {
            let response = match self
                .client
                .post(self.method_url(method))
                .json(body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(_) if attempt < max_attempts => {
                    Self::wait_before_retry(attempt, None).await;
                    attempt += 1;
                    continue;
                }
                Err(err) => {
                    return Err(Self::safe_transport_error(method, "request", &err, true));
                }
            };
            let status = response.status();
            let retryable_status =
                status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            let envelope = match response.json::<TelegramResponse<T>>().await {
                Ok(envelope) => envelope,
                Err(_) if retryable_status && attempt < max_attempts => {
                    Self::wait_before_retry(attempt, None).await;
                    attempt += 1;
                    continue;
                }
                Err(err) => {
                    return Err(Self::safe_transport_error(
                        method,
                        "response",
                        &err,
                        retryable_status,
                    ));
                }
            };
            if envelope.ok {
                return envelope.result.ok_or_else(|| {
                    TelegramApiError::new(
                        false,
                        format!("telegram {method} response missing result"),
                    )
                });
            }

            let error_code = envelope.error_code;
            let retry_after = envelope
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.retry_after);
            let retryable = retryable_status
                || error_code == Some(429)
                || error_code.is_some_and(|code| (500..600).contains(&code));
            if retryable && attempt < max_attempts {
                Self::wait_before_retry(attempt, retry_after).await;
                attempt += 1;
                continue;
            }

            let code = error_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let description = envelope
                .description
                .unwrap_or_else(|| "unknown Telegram API error".to_string());
            return Err(TelegramApiError::new(
                retryable,
                format!("telegram {method} failed code={code}: {description}"),
            ));
        }
    }

    async fn wait_before_retry(attempt: usize, retry_after: Option<u64>) {
        let delay = retry_after.map(Duration::from_secs).unwrap_or_else(|| {
            let multiplier = 1_u64 << attempt.saturating_sub(1).min(3);
            Duration::from_millis(TELEGRAM_REQUEST_RETRY_DELAY_MILLIS.saturating_mul(multiplier))
        });
        tokio::time::sleep(delay).await;
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.token, method)
    }

    fn safe_transport_error(
        method: &str,
        phase: &str,
        err: &reqwest::Error,
        retryable: bool,
    ) -> TelegramApiError {
        let kind = if err.is_timeout() {
            "timed out"
        } else if err.is_connect() {
            "connection failed"
        } else if err.is_decode() {
            "invalid response"
        } else if err.is_request() {
            "invalid request"
        } else {
            "transport failed"
        };
        TelegramApiError::new(
            retryable,
            format!("telegram {method} {phase} failed: {kind}"),
        )
    }

    fn draft_id_seed() -> i64 {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        millis.max(1)
    }

    fn http_client(proxy: Option<&crate::config::HttpProxy>) -> Result<Client> {
        let builder = Client::builder()
            .pool_max_idle_per_host(TELEGRAM_HTTP_MAX_IDLE_CONNECTIONS_PER_HOST)
            .pool_idle_timeout(Some(Duration::from_secs(
                TELEGRAM_HTTP_IDLE_TIMEOUT_SECONDS,
            )))
            .connect_timeout(Duration::from_secs(TELEGRAM_HTTP_CONNECT_TIMEOUT_SECONDS))
            .timeout(Duration::from_secs(TELEGRAM_HTTP_REQUEST_TIMEOUT_SECONDS));
        http::client(builder, proxy).context("build telegram http client failed")
    }
}

#[derive(Debug)]
struct TelegramApiError {
    retryable: bool,
    message: String,
}

impl TelegramApiError {
    fn new(retryable: bool, message: impl Into<String>) -> Self {
        Self {
            retryable,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TelegramApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TelegramApiError {}

#[derive(Debug)]
pub(super) struct TelegramFileDownloadError {
    retryable: bool,
    message: String,
}

impl TelegramFileDownloadError {
    fn new(retryable: bool, message: impl Into<String>) -> Self {
        Self {
            retryable,
            message: message.into(),
        }
    }

    fn from_api(error: TelegramApiError) -> Self {
        Self::new(error.retryable, error.message)
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self::new(false, message)
    }

    pub(super) fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl std::fmt::Display for TelegramFileDownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TelegramFileDownloadError {}

#[derive(Serialize)]
struct EmptyRequest {}

#[derive(Serialize)]
struct GetUpdatesRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<i64>,
    timeout: u64,
    allowed_updates: [&'a str; 2],
}

#[derive(Serialize)]
struct AnswerCallbackQueryRequest<'a> {
    callback_query_id: &'a str,
}

#[derive(Serialize)]
struct SetMyCommandsRequest<'a> {
    commands: &'a [TelegramBotCommand<'a>],
}

#[derive(Serialize)]
struct GetFileRequest<'a> {
    file_id: &'a str,
}

#[derive(Serialize)]
pub(super) struct TelegramBotCommand<'a> {
    command: &'a str,
    description: &'a str,
}

impl<'a> TelegramBotCommand<'a> {
    pub(super) const fn new(command: &'a str, description: &'a str) -> Self {
        Self {
            command,
            description,
        }
    }
}

#[derive(Serialize)]
struct InputRichMessage<'a> {
    markdown: &'a str,
}

#[derive(Serialize)]
struct SendRichMessageDraftRequest<'a> {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    draft_id: i64,
    rich_message: InputRichMessage<'a>,
}

#[derive(Serialize)]
struct SendRichMessageRequest<'a> {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    rich_message: InputRichMessage<'a>,
    reply_parameters: ReplyParameters,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<TelegramInlineKeyboardMarkup<'a>>,
}

#[derive(Serialize)]
struct EditRichMessageRequest<'a> {
    chat_id: i64,
    message_id: i64,
    rich_message: InputRichMessage<'a>,
    reply_markup: TelegramInlineKeyboardMarkup<'a>,
}

#[derive(Serialize)]
struct TelegramInlineKeyboardMarkup<'a> {
    inline_keyboard: Vec<Vec<TelegramInlineKeyboardButton<'a>>>,
}

impl<'a> TelegramInlineKeyboardMarkup<'a> {
    fn stop(callback_data: &'a str) -> Self {
        Self::stop_or_empty(Some(callback_data))
    }

    fn stop_or_empty(callback_data: Option<&'a str>) -> Self {
        let inline_keyboard = callback_data
            .map(|callback_data| {
                vec![vec![TelegramInlineKeyboardButton {
                    text: crate::i18n::STOP_TASK,
                    callback_data,
                    style: "danger",
                }]]
            })
            .unwrap_or_default();
        Self { inline_keyboard }
    }
}

#[derive(Serialize)]
struct TelegramInlineKeyboardButton<'a> {
    text: &'static str,
    callback_data: &'a str,
    style: &'static str,
}

#[derive(Serialize)]
struct ReplyParameters {
    message_id: i64,
}

#[derive(Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i64>,
    description: Option<String>,
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Deserialize)]
struct TelegramResponseParameters {
    retry_after: Option<u64>,
}

#[derive(Deserialize)]
struct TelegramUser {
    username: Option<String>,
}

#[derive(Deserialize)]
struct TelegramFile {
    file_path: Option<String>,
}

pub(super) struct TelegramFileResource {
    pub(super) file_name: String,
    pub(super) media_type: String,
    pub(super) data: Vec<u8>,
}

#[derive(Deserialize)]
struct TelegramSentMessage {
    message_id: i64,
}
