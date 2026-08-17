use super::LarkReplyTarget;
use super::channel::{LarkDelivery, LarkEvent};
use super::proxy;
use crate::config::LarkChannelConfig;
use crate::http;
use agora_core::logger;
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::{client_async_tls, connect_async};

const LARK_OPENAPI: &str = "https://open.feishu.cn";
const LARK_WS_ENDPOINT_PATH: &str = "/callback/ws/endpoint";
const LARK_FRAME_TYPE_CONTROL: i32 = 0;
const LARK_FRAME_TYPE_DATA: i32 = 1;
const LARK_MESSAGE_TYPE_EVENT: &str = "event";
const LARK_MESSAGE_TYPE_PING: &str = "ping";
const DEFAULT_WS_PING_INTERVAL_SECONDS: u64 = 120;
const LARK_RECONNECT_INITIAL_DELAY_SECONDS: u64 = 1;
const LARK_RECONNECT_MAX_DELAY_SECONDS: u64 = 60;
const LARK_HTTP_MAX_IDLE_CONNECTIONS_PER_HOST: usize = 10;
const LARK_HTTP_IDLE_TIMEOUT_SECONDS: u64 = 300;
const LARK_HTTP_CONNECT_TIMEOUT_SECONDS: u64 = 10;
const LARK_HTTP_REQUEST_TIMEOUT_SECONDS: u64 = 60;
const LARK_PATCH_MAX_ATTEMPTS: usize = 3;
const LARK_PATCH_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(100);
const LARK_MAX_IN_FLIGHT_EVENTS: usize = 64;
const LARK_EVENT_CACHE_CAPACITY: usize = 4096;
const LARK_EVENT_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const LARK_PENDING_EVENT_TTL: Duration = Duration::from_secs(2 * 60);
const LARK_WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LARK_WS_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const LARK_WS_MINIMUM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct LarkWebSocketTiming {
    connect_timeout: Duration,
    write_timeout: Duration,
    default_ping_interval: Duration,
    minimum_idle_timeout: Duration,
}

impl Default for LarkWebSocketTiming {
    fn default() -> Self {
        Self {
            connect_timeout: LARK_WS_CONNECT_TIMEOUT,
            write_timeout: LARK_WS_WRITE_TIMEOUT,
            default_ping_interval: Duration::from_secs(DEFAULT_WS_PING_INTERVAL_SECONDS),
            minimum_idle_timeout: LARK_WS_MINIMUM_IDLE_TIMEOUT,
        }
    }
}

#[derive(Clone)]
pub(super) struct LarkApi {
    name: String,
    app_id: String,
    secret: String,
    client: Client,
    base_url: String,
    proxy: Option<crate::config::HttpProxy>,
    event_cache: Arc<Mutex<LarkEventCache>>,
    websocket_timing: LarkWebSocketTiming,
}

#[derive(Default)]
struct LarkEventCache {
    entries: HashMap<String, LarkEventCacheEntry>,
    next_generation: u64,
}

enum LarkEventCacheEntry {
    Pending {
        generation: u64,
        started_at: Instant,
        waiters: Vec<oneshot::Sender<u16>>,
    },
    Completed {
        status_code: u16,
        expires_at: Instant,
    },
}

enum LarkEventCacheDecision {
    Execute(u64),
    Wait(oneshot::Receiver<u16>),
    Replay(u16),
    Overloaded,
}

impl LarkEventCache {
    fn acquire(&mut self, event_id: &str) -> LarkEventCacheDecision {
        let now = Instant::now();
        self.entries.retain(|_, entry| match entry {
            LarkEventCacheEntry::Pending { started_at, .. } => {
                now.duration_since(*started_at) < LARK_PENDING_EVENT_TTL
            }
            LarkEventCacheEntry::Completed { expires_at, .. } => *expires_at > now,
        });

        if let Some(entry) = self.entries.get_mut(event_id) {
            return match entry {
                LarkEventCacheEntry::Pending { waiters, .. } => {
                    let (sender, receiver) = oneshot::channel();
                    waiters.push(sender);
                    LarkEventCacheDecision::Wait(receiver)
                }
                LarkEventCacheEntry::Completed { status_code, .. } => {
                    LarkEventCacheDecision::Replay(*status_code)
                }
            };
        }

        if self.entries.len() >= LARK_EVENT_CACHE_CAPACITY {
            let oldest_completed = self
                .entries
                .iter()
                .filter_map(|(event_id, entry)| match entry {
                    LarkEventCacheEntry::Completed { expires_at, .. } => {
                        Some((event_id.clone(), *expires_at))
                    }
                    LarkEventCacheEntry::Pending { .. } => None,
                })
                .min_by_key(|(_, expires_at)| *expires_at)
                .map(|(event_id, _)| event_id);
            if let Some(event_id) = oldest_completed {
                self.entries.remove(&event_id);
            }
        }
        if self.entries.len() >= LARK_EVENT_CACHE_CAPACITY {
            return LarkEventCacheDecision::Overloaded;
        }

        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        self.entries.insert(
            event_id.to_string(),
            LarkEventCacheEntry::Pending {
                generation,
                started_at: now,
                waiters: Vec::new(),
            },
        );
        LarkEventCacheDecision::Execute(generation)
    }

    fn complete(&mut self, event_id: &str, generation: u64, status_code: u16) {
        let Some(entry) = self.entries.remove(event_id) else {
            return;
        };
        let LarkEventCacheEntry::Pending {
            generation: pending_generation,
            started_at,
            waiters,
        } = entry
        else {
            self.entries.insert(event_id.to_string(), entry);
            return;
        };
        if pending_generation != generation {
            self.entries.insert(
                event_id.to_string(),
                LarkEventCacheEntry::Pending {
                    generation: pending_generation,
                    started_at,
                    waiters,
                },
            );
            return;
        }

        for waiter in waiters {
            let _ = waiter.send(status_code);
        }
        if status_code == 200 {
            self.entries.insert(
                event_id.to_string(),
                LarkEventCacheEntry::Completed {
                    status_code,
                    expires_at: Instant::now() + LARK_EVENT_CACHE_TTL,
                },
            );
        }
    }
}

pub(super) struct LarkImageResource {
    pub(super) media_type: String,
    pub(super) data: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct LarkImageDownloadError {
    message: String,
    permanent: bool,
}

impl LarkImageDownloadError {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: false,
        }
    }

    pub(super) fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: true,
        }
    }

    pub(super) fn is_permanent(&self) -> bool {
        self.permanent
    }
}

impl std::fmt::Display for LarkImageDownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LarkImageDownloadError {}

#[derive(Debug)]
pub(super) struct LarkHttpStatusError(StatusCode);

impl LarkHttpStatusError {
    pub(super) fn is_unauthorized(&self) -> bool {
        self.0 == StatusCode::UNAUTHORIZED
    }
}

impl std::fmt::Display for LarkHttpStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "lark HTTP request failed: {}", self.0)
    }
}

impl std::error::Error for LarkHttpStatusError {}

impl LarkApi {
    pub(super) fn new(config: LarkChannelConfig) -> Result<Self> {
        Self::with_base_url(config, LARK_OPENAPI.to_string())
    }

    pub(super) fn with_base_url(config: LarkChannelConfig, base_url: String) -> Result<Self> {
        let client = Self::http_client(config.proxy.as_ref())?;
        Ok(Self {
            name: config.name,
            app_id: config.app_id,
            secret: config.secret,
            client,
            base_url,
            proxy: config.proxy,
            event_cache: Arc::new(Mutex::new(LarkEventCache::default())),
            websocket_timing: LarkWebSocketTiming::default(),
        })
    }

    #[cfg(test)]
    fn with_websocket_timing(mut self, timing: LarkWebSocketTiming) -> Self {
        self.websocket_timing = timing;
        self
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) async fn run_websocket_loop(
        &self,
        events: mpsc::Sender<LarkDelivery>,
    ) -> Result<()> {
        let mut backoff = LarkReconnectBackoff::default();
        logger::info!("lark channel starting channel={}", self.name);
        loop {
            let mut connected = false;
            logger::info!("lark websocket connecting channel={}", self.name);
            match self
                .run_websocket_once(events.clone(), &mut connected)
                .await
            {
                Ok(()) => {
                    logger::info!(
                        "lark websocket disconnected channel={}, reconnecting",
                        self.name
                    );
                }
                Err(_) => {
                    if connected {
                        logger::error!(
                            "lark websocket disconnected channel={} reason=connection_error",
                            self.name
                        );
                    } else {
                        logger::error!(
                            "lark channel startup failed channel={} reason=connection_error",
                            self.name
                        );
                    }
                }
            }
            if events.is_closed() {
                return Err(anyhow!("agora lark receiver closed"));
            }

            let delay = backoff.next_delay_after_attempt(connected);
            logger::info!(
                "lark websocket reconnect scheduled channel={} delay_secs={}",
                self.name,
                delay.as_secs()
            );
            tokio::time::sleep(delay).await;
        }
    }

    async fn run_websocket_once(
        &self,
        events: mpsc::Sender<LarkDelivery>,
        connected: &mut bool,
    ) -> Result<()> {
        let (endpoint_url, client_config) = self.websocket_endpoint().await?;
        let service_id = Self::query_param(&endpoint_url, "service_id")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or_default();
        let ping_interval_duration = if client_config.ping_interval > 0 {
            Duration::from_secs(client_config.ping_interval as u64)
        } else {
            self.websocket_timing.default_ping_interval
        };
        let idle_timeout = ping_interval_duration
            .saturating_mul(2)
            .max(self.websocket_timing.minimum_idle_timeout);

        let connect = async {
            let connected = match &self.proxy {
                Some(proxy) => {
                    let stream = proxy::connect_tunnel(proxy, &endpoint_url).await?;
                    client_async_tls(endpoint_url.as_str(), stream).await?
                }
                None => connect_async(endpoint_url.as_str()).await?,
            };
            Ok::<_, anyhow::Error>(connected)
        };
        let (mut socket, _) = tokio::time::timeout(self.websocket_timing.connect_timeout, connect)
            .await
            .map_err(|_| anyhow!("connect lark websocket timed out"))?
            .context("connect lark websocket failed")?;
        *connected = true;
        logger::info!("lark websocket connected channel={}", self.name);
        let mut ping_interval = tokio::time::interval(ping_interval_duration);
        let inbound_idle = tokio::time::sleep(idle_timeout);
        tokio::pin!(inbound_idle);
        let in_flight = Arc::new(Semaphore::new(LARK_MAX_IN_FLIGHT_EVENTS));
        let (acknowledgements, mut acknowledged) =
            mpsc::channel::<Result<LarkFrame>>(LARK_MAX_IN_FLIGHT_EVENTS);

        loop {
            tokio::select! {
                acknowledgement = acknowledged.recv() => {
                    let acknowledgement = acknowledgement
                        .expect("lark acknowledgement sender is retained while connected")?;
                    self.write_websocket(
                        socket.send(WebSocketMessage::Binary(
                            acknowledgement.encode_to_vec().into(),
                        )),
                        "send lark websocket ack failed",
                    )
                    .await?;
                }
                message = socket.next() => {
                    let Some(message) = message else {
                        return Ok(());
                    };
                    let message = message.context("read lark websocket message failed")?;
                    inbound_idle
                        .as_mut()
                        .reset(tokio::time::Instant::now() + idle_timeout);
                    match message {
                        WebSocketMessage::Binary(payload) => {
                            let frame = LarkFrame::decode(payload)
                                .context("decode lark websocket frame failed")?;
                            if frame.method == LARK_FRAME_TYPE_DATA
                                && frame.header("type") == Some(LARK_MESSAGE_TYPE_EVENT)
                            {
                                let Ok(permit) = Arc::clone(&in_flight).try_acquire_owned() else {
                                    let ack = frame.into_ack(500, 0)?;
                                    self.write_websocket(
                                        socket.send(WebSocketMessage::Binary(
                                            ack.encode_to_vec().into(),
                                        )),
                                        "send overloaded lark websocket ack failed",
                                    )
                                    .await?;
                                    continue;
                                };
                                let api = self.clone();
                                let events = events.clone();
                                let acknowledgements = acknowledgements.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    match api.handle_data_frame(frame, &events).await {
                                        Ok(Some(acknowledgement)) => {
                                            let _ = acknowledgements.send(Ok(acknowledgement)).await;
                                        }
                                        Ok(None) => {}
                                        Err(error) => {
                                            let _ = acknowledgements.send(Err(error)).await;
                                        }
                                    }
                                });
                            }
                        }
                        WebSocketMessage::Ping(payload) => {
                            self.write_websocket(
                                socket.send(WebSocketMessage::Pong(payload)),
                                "send lark websocket pong failed",
                            )
                            .await?;
                        }
                        WebSocketMessage::Close(_) => return Ok(()),
                        _ => {}
                    }
                }
                _ = ping_interval.tick() => {
                    let ping = LarkFrame::ping(service_id);
                    self.write_websocket(
                        socket.send(WebSocketMessage::Binary(ping.encode_to_vec().into())),
                        "send lark websocket ping failed",
                    )
                    .await?;
                }
                _ = &mut inbound_idle => {
                    return Err(anyhow!("lark websocket inbound idle timed out"));
                }
            }
        }
    }

    async fn write_websocket<F>(&self, write: F, failure_context: &str) -> Result<()>
    where
        F: Future<Output = std::result::Result<(), tokio_tungstenite::tungstenite::Error>>,
    {
        tokio::time::timeout(self.websocket_timing.write_timeout, write)
            .await
            .map_err(|_| anyhow!("write lark websocket timed out: {failure_context}"))?
            .with_context(|| failure_context.to_string())
    }

    async fn websocket_endpoint(&self) -> Result<(String, LarkWebSocketClientConfig)> {
        let url = format!("{}{LARK_WS_ENDPOINT_PATH}", self.base_url);
        let response = self
            .client
            .post(url)
            .header("locale", "zh")
            .json(&json!({
                "AppID": self.app_id,
                "AppSecret": self.secret,
            }))
            .send()
            .await
            .context("request lark websocket endpoint failed")?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("lark websocket endpoint http failed: {status}"));
        }
        let endpoint = response
            .json::<LarkWebSocketEndpointResponse>()
            .await
            .context("parse lark websocket endpoint response failed")?;
        if endpoint.code != 0 {
            return Err(anyhow!(
                "lark websocket endpoint failed: code={}, msg={}",
                endpoint.code,
                endpoint.msg
            ));
        }
        let data = endpoint
            .data
            .ok_or_else(|| anyhow!("lark websocket endpoint response missing data"))?;
        Ok((data.url, data.client_config.unwrap_or_default()))
    }

    fn http_client(proxy: Option<&crate::config::HttpProxy>) -> Result<Client> {
        let builder = Client::builder()
            .pool_max_idle_per_host(LARK_HTTP_MAX_IDLE_CONNECTIONS_PER_HOST)
            .pool_idle_timeout(Some(Duration::from_secs(LARK_HTTP_IDLE_TIMEOUT_SECONDS)))
            .connect_timeout(Duration::from_secs(LARK_HTTP_CONNECT_TIMEOUT_SECONDS))
            .timeout(Duration::from_secs(LARK_HTTP_REQUEST_TIMEOUT_SECONDS));
        http::client(builder, proxy).context("build lark http client failed")
    }

    fn query_param(url: &str, key: &str) -> Option<String> {
        let query = url.split_once('?')?.1;
        query.split('&').find_map(|part| {
            let (name, value) = part.split_once('=')?;
            (name == key).then(|| value.to_string())
        })
    }

    #[cfg(test)]
    async fn handle_websocket_binary(
        &self,
        payload: &[u8],
        events: &mpsc::Sender<LarkDelivery>,
    ) -> Result<Option<LarkFrame>> {
        let frame = LarkFrame::decode(payload).context("decode lark websocket frame failed")?;
        match frame.method {
            LARK_FRAME_TYPE_CONTROL => Ok(None),
            LARK_FRAME_TYPE_DATA => self.handle_data_frame(frame, events).await,
            _ => Ok(None),
        }
    }

    async fn handle_data_frame(
        &self,
        frame: LarkFrame,
        events: &mpsc::Sender<LarkDelivery>,
    ) -> Result<Option<LarkFrame>> {
        if frame.header("type") != Some(LARK_MESSAGE_TYPE_EVENT) {
            return Ok(None);
        }

        let started = Instant::now();
        let status_code = match LarkEvent::from_lark_event_payload(&frame.payload) {
            Ok(
                event
                @ (LarkEvent::Message(_) | LarkEvent::CardAction(_) | LarkEvent::Interrupt(_)),
            ) => {
                let event_id = event
                    .id()
                    .expect("deliverable lark events always carry an event id")
                    .to_string();
                let decision = {
                    let mut cache = self.event_cache.lock().await;
                    cache.acquire(&event_id)
                };
                match decision {
                    LarkEventCacheDecision::Execute(generation) => {
                        let admitted = self.deliver_event(event, events).await;
                        let status_code = admitted.as_ref().copied().unwrap_or(500);
                        self.event_cache
                            .lock()
                            .await
                            .complete(&event_id, generation, status_code);
                        admitted?
                    }
                    LarkEventCacheDecision::Wait(acknowledged) => acknowledged.await.unwrap_or(500),
                    LarkEventCacheDecision::Replay(status_code) => status_code,
                    LarkEventCacheDecision::Overloaded => 500,
                }
            }
            Ok(LarkEvent::Ignore { .. }) => 200,
            Err(err) => {
                logger::error!("ignore invalid lark event payload: {}", err);
                200
            }
        };
        Ok(Some(
            frame.into_ack(status_code, started.elapsed().as_millis())?,
        ))
    }

    async fn deliver_event(
        &self,
        event: LarkEvent,
        events: &mpsc::Sender<LarkDelivery>,
    ) -> Result<u16> {
        let (delivery, acknowledged) = LarkDelivery::new(event);
        let deadline = delivery.deadline();
        match tokio::time::timeout_at(deadline, async {
            events
                .send(delivery)
                .await
                .map_err(|_| anyhow!("agora lark receiver closed"))?;
            Ok(acknowledged.await.unwrap_or(500))
        })
        .await
        {
            Ok(status_code) => status_code,
            Err(_) => Ok(500),
        }
    }

    pub(super) async fn tenant_access_token(&self) -> Result<String> {
        let response = self
            .client
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.base_url
            ))
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.secret,
            }))
            .send()
            .await?
            .json::<TenantTokenResponse>()
            .await?;
        response.into_result()
    }

    pub(super) async fn bot_open_id(&self) -> Result<String> {
        let token = self.tenant_access_token().await?;
        let response = self
            .client
            .get(format!("{}/open-apis/bot/v3/info", self.base_url))
            .bearer_auth(token)
            .send()
            .await?
            .json::<LarkBotInfoResponse>()
            .await?;
        response.into_result()
    }

    pub(super) async fn download_message_image(
        &self,
        token: &str,
        message_id: &str,
        image_key: &str,
        maximum_bytes: usize,
    ) -> std::result::Result<LarkImageResource, LarkImageDownloadError> {
        let response = self
            .client
            .get(format!(
                "{}/open-apis/im/v1/messages/{}/resources/{}",
                self.base_url, message_id, image_key
            ))
            .query(&[("type", "image")])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| LarkImageDownloadError::transient(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = format!("download lark message image http failed: {status}");
            return Err(
                if status.is_client_error()
                    && status != StatusCode::REQUEST_TIMEOUT
                    && status != StatusCode::TOO_MANY_REQUESTS
                {
                    LarkImageDownloadError::permanent(message)
                } else {
                    LarkImageDownloadError::transient(message)
                },
            );
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
            .map_err(|error| {
                let message = format!("read lark message image failed: {error}");
                if error.is_limit_exceeded() {
                    LarkImageDownloadError::permanent(message)
                } else {
                    LarkImageDownloadError::transient(message)
                }
            })?;
        Ok(LarkImageResource { media_type, data })
    }

    pub(super) async fn reply_card(
        &self,
        token: &str,
        target: &LarkReplyTarget,
        card: &Value,
    ) -> Result<String> {
        self.reply_message(token, target, "interactive", serde_json::to_string(card)?)
            .await
    }

    pub(super) async fn reply_text(
        &self,
        token: &str,
        target: &LarkReplyTarget,
        text: &str,
    ) -> Result<()> {
        self.reply_message(
            token,
            target,
            "text",
            serde_json::to_string(&json!({ "text": text }))?,
        )
        .await?;
        Ok(())
    }

    async fn reply_message(
        &self,
        token: &str,
        target: &LarkReplyTarget,
        msg_type: &str,
        content: String,
    ) -> Result<String> {
        let response = self
            .client
            .post(format!(
                "{}/open-apis/im/v1/messages/{}/reply",
                self.base_url, target.message_id
            ))
            .bearer_auth(token)
            .json(&ReplyMessageRequest {
                msg_type,
                content,
                reply_in_thread: true,
            })
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(LarkHttpStatusError(response.status()).into());
        }
        let response = response.json::<SendCardResponse>().await?;
        response.into_result()
    }

    pub(super) async fn patch_card(
        &self,
        token: &str,
        message_id: &str,
        card: &Value,
    ) -> Result<()> {
        let url = format!("{}/open-apis/im/v1/messages/{}", self.base_url, message_id);
        let content = serde_json::to_string(card)?;
        let mut delay = LARK_PATCH_RETRY_INITIAL_DELAY;

        for attempt in 1..=LARK_PATCH_MAX_ATTEMPTS {
            match self
                .client
                .patch(&url)
                .bearer_auth(token)
                .json(&PatchCardRequest {
                    content: content.clone(),
                })
                .send()
                .await
            {
                Ok(response)
                    if attempt < LARK_PATCH_MAX_ATTEMPTS
                        && (response.status().is_server_error()
                            || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS) => {}
                Ok(response) => {
                    if !response.status().is_success() {
                        return Err(LarkHttpStatusError(response.status()).into());
                    }
                    return response.json::<LarkEmptyResponse>().await?.into_result();
                }
                Err(error) if attempt < LARK_PATCH_MAX_ATTEMPTS => {
                    logger::debug!("lark card patch retry attempt={} error={}", attempt, error);
                }
                Err(error) => return Err(error.into()),
            }

            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
        }

        Err(anyhow!("lark card patch attempts exhausted"))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(super) struct LarkWebSocketEndpointResponse {
    pub(super) code: i32,
    #[serde(default)]
    pub(super) msg: String,
    pub(super) data: Option<LarkWebSocketEndpoint>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub(super) struct LarkWebSocketEndpoint {
    #[serde(rename = "URL")]
    pub(super) url: String,
    #[serde(default)]
    pub(super) client_config: Option<LarkWebSocketClientConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub(super) struct LarkWebSocketClientConfig {
    #[serde(default)]
    pub(super) reconnect_count: i32,
    #[serde(default)]
    pub(super) reconnect_interval: i32,
    #[serde(default)]
    pub(super) reconnect_nonce: i32,
    #[serde(default)]
    pub(super) ping_interval: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LarkReconnectBackoff {
    next_delay: Duration,
}

impl Default for LarkReconnectBackoff {
    fn default() -> Self {
        Self {
            next_delay: Duration::from_secs(LARK_RECONNECT_INITIAL_DELAY_SECONDS),
        }
    }
}

impl LarkReconnectBackoff {
    pub(super) fn next_delay(&mut self) -> Duration {
        let delay = self.next_delay;
        self.next_delay = self
            .next_delay
            .saturating_mul(2)
            .min(Duration::from_secs(LARK_RECONNECT_MAX_DELAY_SECONDS));
        delay
    }

    pub(super) fn reset(&mut self) {
        self.next_delay = Duration::from_secs(LARK_RECONNECT_INITIAL_DELAY_SECONDS);
    }

    pub(super) fn next_delay_after_attempt(&mut self, connected: bool) -> Duration {
        if connected {
            self.reset();
        }
        self.next_delay()
    }
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct LarkFrameHeader {
    #[prost(string, tag = "1")]
    pub(super) key: String,
    #[prost(string, tag = "2")]
    pub(super) value: String,
}

impl LarkFrameHeader {
    pub(super) fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct LarkFrame {
    #[prost(uint64, tag = "1")]
    pub(super) seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub(super) log_id: u64,
    #[prost(int32, tag = "3")]
    pub(super) service: i32,
    #[prost(int32, tag = "4")]
    pub(super) method: i32,
    #[prost(message, repeated, tag = "5")]
    pub(super) headers: Vec<LarkFrameHeader>,
    #[prost(string, tag = "6")]
    pub(super) payload_encoding: String,
    #[prost(string, tag = "7")]
    pub(super) payload_type: String,
    #[prost(bytes, tag = "8")]
    pub(super) payload: Vec<u8>,
    #[prost(string, tag = "9")]
    pub(super) log_id_new: String,
}

impl LarkFrame {
    pub(super) fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.key == key)
            .map(|header| header.value.as_str())
    }

    pub(super) fn into_ack(mut self, status_code: u16, biz_rt_ms: u128) -> Result<Self> {
        self.upsert_header("biz_rt", biz_rt_ms.to_string());
        self.payload = serde_json::to_vec(&LarkWebSocketAck {
            code: status_code,
            headers: None,
            data: None,
        })?;
        Ok(self)
    }

    fn ping(service_id: i32) -> Self {
        Self {
            seq_id: 0,
            log_id: 0,
            service: service_id,
            method: LARK_FRAME_TYPE_CONTROL,
            headers: vec![LarkFrameHeader::new("type", LARK_MESSAGE_TYPE_PING)],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: Vec::new(),
            log_id_new: String::new(),
        }
    }

    fn upsert_header(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if let Some(header) = self.headers.iter_mut().find(|header| header.key == key) {
            header.value = value;
        } else {
            self.headers.push(LarkFrameHeader::new(key, value));
        }
    }
}

#[derive(Serialize)]
struct LarkWebSocketAck {
    code: u16,
    headers: Option<BTreeMap<String, String>>,
    data: Option<Value>,
}

#[derive(Deserialize)]
struct TenantTokenResponse {
    code: i32,
    msg: String,
    tenant_access_token: Option<String>,
}

impl TenantTokenResponse {
    fn into_result(self) -> Result<String> {
        if self.code == 0 {
            self.tenant_access_token
                .ok_or_else(|| anyhow!("lark response missing tenant_access_token"))
        } else {
            Err(anyhow!("lark tenant token failed: {}", self.msg))
        }
    }
}

#[derive(Deserialize)]
struct LarkBotInfoResponse {
    code: i32,
    msg: String,
    bot: Option<LarkBotInfo>,
}

#[derive(Deserialize)]
struct LarkBotInfo {
    open_id: String,
}

impl LarkBotInfoResponse {
    fn into_result(self) -> Result<String> {
        if self.code == 0 {
            self.bot
                .map(|bot| bot.open_id)
                .filter(|open_id| !open_id.is_empty())
                .ok_or_else(|| anyhow!("lark bot info response missing open_id"))
        } else {
            Err(anyhow!("lark bot info failed: {}", self.msg))
        }
    }
}

#[derive(Serialize)]
struct ReplyMessageRequest<'a> {
    msg_type: &'a str,
    content: String,
    reply_in_thread: bool,
}

#[derive(Deserialize)]
struct SendCardResponse {
    code: i32,
    msg: String,
    data: Option<SendCardData>,
}

#[derive(Deserialize)]
struct SendCardData {
    message_id: String,
}

impl SendCardResponse {
    fn into_result(self) -> Result<String> {
        if self.code == 0 {
            self.data
                .map(|data| data.message_id)
                .ok_or_else(|| anyhow!("lark response missing message_id"))
        } else {
            Err(anyhow!("lark reply message failed: {}", self.msg))
        }
    }
}

#[derive(Serialize)]
struct PatchCardRequest {
    content: String,
}

#[derive(Deserialize)]
struct LarkEmptyResponse {
    code: i32,
    msg: String,
}

impl LarkEmptyResponse {
    fn into_result(self) -> Result<()> {
        if self.code == 0 {
            Ok(())
        } else {
            Err(anyhow!("lark patch card failed: {}", self.msg))
        }
    }
}

#[cfg(test)]
mod tests;
