use crate::config::HttpProxy;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const MAX_CONNECT_RESPONSE_SIZE: usize = 8192;
const LARK_PROXY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) async fn connect_tunnel(proxy: &HttpProxy, target_url: &str) -> Result<TcpStream> {
    connect_tunnel_with_timeout(proxy, target_url, LARK_PROXY_CONNECT_TIMEOUT).await
}

async fn connect_tunnel_with_timeout(
    proxy: &HttpProxy,
    target_url: &str,
    connect_timeout: std::time::Duration,
) -> Result<TcpStream> {
    timeout(connect_timeout, connect_tunnel_inner(proxy, target_url))
        .await
        .map_err(|_| anyhow!("HTTP proxy CONNECT timed out after {connect_timeout:?}"))?
}

async fn connect_tunnel_inner(proxy: &HttpProxy, target_url: &str) -> Result<TcpStream> {
    let target = reqwest::Url::parse(target_url).context("parse websocket URL failed")?;
    let host = target
        .host_str()
        .ok_or_else(|| anyhow!("websocket URL is missing a host"))?;
    let port = target
        .port_or_known_default()
        .ok_or_else(|| anyhow!("websocket URL is missing a port"))?;
    let authority = if host.starts_with('[') && host.ends_with(']') {
        format!("{host}:{port}")
    } else if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };

    let mut stream = TcpStream::connect(proxy.address())
        .await
        .context("connect HTTP proxy failed")?;
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some((username, password)) = proxy.credentials() {
        let credentials = STANDARD.encode(format!("{username}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {credentials}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .context("write HTTP proxy CONNECT request failed")?;

    let mut response = Vec::with_capacity(512);
    let header_end = loop {
        let mut buffer = [0_u8; 512];
        let read = stream
            .read(&mut buffer)
            .await
            .context("read HTTP proxy CONNECT response failed")?;
        if read == 0 {
            bail!("HTTP proxy closed before completing CONNECT");
        }
        response.extend_from_slice(&buffer[..read]);
        if response.len() > MAX_CONNECT_RESPONSE_SIZE {
            bail!("HTTP proxy CONNECT response is too large");
        }
        if let Some(index) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let status = std::str::from_utf8(&response[..header_end])
        .context("HTTP proxy CONNECT response is not valid UTF-8")?
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("HTTP proxy CONNECT response is invalid"))?;
    if !(200..300).contains(&status) {
        bail!("HTTP proxy CONNECT failed with status {status}");
    }
    Ok(stream)
}

#[cfg(test)]
mod tests;
