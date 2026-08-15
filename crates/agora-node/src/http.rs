use crate::config::HttpProxy;
use anyhow::{Context, Result};
use reqwest::{Client, ClientBuilder, Response};

pub(crate) const MAX_TASK_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn client(builder: ClientBuilder, proxy: Option<&HttpProxy>) -> Result<Client> {
    let builder = match proxy {
        Some(proxy) => {
            let mut configured = reqwest::Proxy::all(format!("http://{}", proxy.address()))
                .context("configure HTTP proxy failed")?;
            if let Some((username, password)) = proxy.credentials() {
                configured = configured.basic_auth(username, password);
            }
            builder.proxy(configured)
        }
        None => builder,
    };
    #[cfg(test)]
    let builder = if proxy.is_none() {
        builder.no_proxy()
    } else {
        builder
    };
    builder.build().context("build HTTP client failed")
}

pub(crate) async fn read_body_limited(
    mut response: Response,
    maximum: usize,
) -> std::result::Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(BodyReadError::LimitExceeded { maximum });
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(maximum);
    let mut data = Vec::with_capacity(capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BodyReadError::ReadFailed)?
    {
        if chunk.len() > maximum.saturating_sub(data.len()) {
            return Err(BodyReadError::LimitExceeded { maximum });
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

#[derive(Debug)]
pub(crate) enum BodyReadError {
    LimitExceeded { maximum: usize },
    ReadFailed,
}

impl BodyReadError {
    pub(crate) fn is_limit_exceeded(&self) -> bool {
        matches!(self, Self::LimitExceeded { .. })
    }
}

impl std::fmt::Display for BodyReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LimitExceeded { maximum } => write!(
                formatter,
                "HTTP response body limit exceeded: maximum {maximum} bytes"
            ),
            Self::ReadFailed => formatter.write_str("HTTP response body read failed"),
        }
    }
}

impl std::error::Error for BodyReadError {}
