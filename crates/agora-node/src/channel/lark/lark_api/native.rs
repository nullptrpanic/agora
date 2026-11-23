use super::super::LarkReplyTarget;
use super::{LarkApi, LarkApiError};
use crate::channel::bounded_tail;
use crate::i18n;
use anyhow::{Context, Result, ensure};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const MESSAGE_MAX_BYTES: usize = 30_000;

#[derive(Clone, Debug, Deserialize)]
pub(in crate::channel::lark) struct CotRef {
    pub message_id: String,
    pub cot_id: String,
}

#[derive(Deserialize)]
struct NativeResponse {
    code: i32,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Value,
}

impl LarkApi {
    fn native_url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = Url::parse(&self.base_url)?;
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("invalid lark API base"))?
            .extend(["open-apis", "im", "v1"])
            .extend(segments);
        Ok(url)
    }

    // Preserve the exact request across retries. COT events deduplicate by
    // type/content/timestamp; ordinary replies use a stable per-message uuid.
    async fn native_request(
        &self,
        method: Method,
        url: Url,
        body: &Value,
        retry: bool,
    ) -> Result<Value> {
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            let token = self.tenant_access_token().await?;
            let result: Result<Value> = async {
                let response = self
                    .client
                    .request(method.clone(), url.clone())
                    .bearer_auth(&token)
                    .json(body)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await?;
                if !response.status().is_success() {
                    return Err(LarkApiError::Http(response.status()).into());
                }
                let response = response.json::<NativeResponse>().await?;
                if response.code != 0 {
                    return Err(LarkApiError::Business {
                        code: response.code,
                        message: response.msg,
                    }
                    .into());
                }
                Ok(response.data)
            }
            .await;
            match result {
                Err(error)
                    if !refreshed
                        && error
                            .downcast_ref::<LarkApiError>()
                            .is_some_and(LarkApiError::is_unauthorized) =>
                {
                    self.invalidate_cached_tenant_access_token(&token).await;
                    refreshed = true;
                }
                Err(error) if retry && attempt < 2 && retryable(&error) => {
                    tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    pub(in crate::channel::lark) async fn create_cot(
        &self,
        chat_id: &str,
        source_id: &str,
    ) -> Result<CotRef> {
        let mut url = self.native_url(&["message_cot"])?;
        url.query_pairs_mut()
            .append_pair("receive_id_type", "chat_id");
        let data = self
            .native_request(
                Method::POST,
                url,
                &json!({
                    "receive_id": chat_id, "origin_message_id": source_id, "reply_in_thread": false,
                    "enable_badge": false, "update_feed_rank": false, "cot_hidden": false
                }),
                false,
            )
            .await?;
        let reference: CotRef =
            serde_json::from_value(data).context("lark COT response missing ids")?;
        ensure!(
            !reference.cot_id.is_empty() && !reference.message_id.is_empty(),
            "lark COT response has empty ids"
        );
        Ok(reference)
    }

    pub(in crate::channel::lark) async fn append_cot(
        &self,
        reference: &CotRef,
        events: &[Value],
    ) -> Result<()> {
        ensure!(
            !events.is_empty() && events.len() <= 50,
            "invalid lark COT batch size"
        );
        ensure!(
            events.iter().all(|event| event.to_string().len() <= 4096),
            "lark COT event exceeds byte limit"
        );
        let body = json!({"cot_id": reference.cot_id, "message_id": reference.message_id, "events": events});
        ensure!(
            body.to_string().len() <= 256 * 1024,
            "lark COT batch exceeds byte limit"
        );
        self.native_request(Method::PUT, self.native_url(&["message_cot"])?, &body, true)
            .await?;
        Ok(())
    }

    pub(in crate::channel::lark) async fn complete_cot(
        &self,
        reference: &CotRef,
        success: bool,
    ) -> Result<()> {
        let mut url = self.native_url(&["message_cot", "complete", &reference.cot_id])?;
        url.query_pairs_mut()
            .append_pair("message_id", &reference.message_id)
            .append_pair("reason", if success { "done" } else { "error" });
        self.native_request(Method::POST, url, &json!({}), true)
            .await?;
        Ok(())
    }

    pub(in crate::channel::lark) async fn reply_post(
        &self,
        target: &LarkReplyTarget,
        text: &str,
        uuid: &str,
    ) -> Result<String> {
        let body = post_body(text, Some(uuid), target)?;
        let data = self
            .native_request(
                Method::POST,
                self.native_url(&["messages", &target.message_id, "reply"])?,
                &body,
                true,
            )
            .await?;
        data["message_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .context("lark post response missing message_id")
    }

    pub(in crate::channel::lark) async fn update_post(
        &self,
        message_id: &str,
        text: &str,
        target: &LarkReplyTarget,
    ) -> Result<()> {
        self.native_request(
            Method::PUT,
            self.native_url(&["messages", message_id])?,
            &post_body(text, None, target)?,
            true,
        )
        .await?;
        Ok(())
    }
}

fn post_body(text: &str, uuid: Option<&str>, target: &LarkReplyTarget) -> Result<Value> {
    let sender_id = target.mention_id()?;
    let mut text = bounded_post_text(text, 20 * 1024);
    loop {
        let mut rows = vec![json!([{"tag":"md", "text":text}])];
        if let Some(sender_id) = sender_id {
            rows.insert(0, json!([{"tag":"at", "user_id":sender_id}]));
        }
        let content = json!({"zh_cn": {"title": "", "content": rows}}).to_string();
        let mut body = json!({"msg_type":"post", "content": content});
        if let Some(uuid) = uuid {
            body["reply_in_thread"] = json!(false);
            body["uuid"] = json!(uuid);
        }
        if body.to_string().len() <= MESSAGE_MAX_BYTES {
            return Ok(body);
        }
        text = bounded_post_text(&text, text.len() * 3 / 4);
    }
}

fn bounded_post_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    // Keep lifecycle/partial-answer notices at the front as well as the newest
    // answer tail. Account for both layers of JSON escaping in post_body.
    let mut prefix = 512.min(limit / 2).min(text.len());
    while !text.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let tail = bounded_tail(
        text[prefix..].to_owned(),
        limit - prefix,
        i18n::OUTPUT_TRUNCATED,
    )
    .0;
    format!("{}{tail}", &text[..prefix])
}

pub(in crate::channel::lark) fn retryable(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<LarkApiError>() {
        Some(LarkApiError::Http(status)) => {
            status.is_server_error() || matches!(status.as_u16(), 408 | 429)
        }
        Some(LarkApiError::Business { code, .. }) => matches!(code, 10020 | 99991400),
        None => error.downcast_ref::<reqwest::Error>().is_some(),
    }
}
