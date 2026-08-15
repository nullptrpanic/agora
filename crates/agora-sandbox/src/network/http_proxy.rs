use crate::callback::HttpProxy;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_RESPONSE_HEAD_SIZE: usize = 16 * 1024;
const MAX_RESPONSE_HEADERS: usize = 64;

pub(super) struct HttpProxyConnection {
    pub(super) stream: TcpStream,
    pub(super) initial_data: Vec<u8>,
}

pub(super) struct HttpProxyConnector;

impl HttpProxyConnector {
    pub(super) async fn connect(
        proxy: &HttpProxy,
        destination: SocketAddr,
    ) -> io::Result<HttpProxyConnection> {
        if proxy.address.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP proxy address must not be empty",
            ));
        }
        if proxy
            .basic_auth
            .as_ref()
            .is_some_and(|auth| auth.username.contains(':'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP proxy Basic Auth username must not contain ':'",
            ));
        }

        let mut stream = TcpStream::connect(proxy.address.as_str()).await?;
        let authority = Self::authority(destination);
        let mut request = format!(
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
        );
        if let Some(auth) = &proxy.basic_auth {
            let credentials = STANDARD.encode(format!("{}:{}", auth.username, auth.password));
            request.push_str("Proxy-Authorization: Basic ");
            request.push_str(&credentials);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await?;

        let initial_data = Self::read_response(&mut stream).await?;
        Ok(HttpProxyConnection {
            stream,
            initial_data,
        })
    }

    async fn read_response(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(1024);
        let mut buffer = [0_u8; 1024];
        loop {
            if bytes.len() == MAX_RESPONSE_HEAD_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP proxy response head is too large",
                ));
            }
            let available = (MAX_RESPONSE_HEAD_SIZE - bytes.len()).min(buffer.len());
            let read = stream.read(&mut buffer[..available]).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "HTTP proxy closed before completing the CONNECT response",
                ));
            }
            bytes.extend_from_slice(&buffer[..read]);

            let mut headers = [httparse::EMPTY_HEADER; MAX_RESPONSE_HEADERS];
            let mut response = httparse::Response::new(&mut headers);
            let consumed = match response.parse(&bytes) {
                Ok(httparse::Status::Complete(consumed)) => consumed,
                Ok(httparse::Status::Partial) => continue,
                Err(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid HTTP proxy response: {error}"),
                    ));
                }
            };
            let status = response.code.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP proxy response has no status code",
                )
            })?;
            if !(200..300).contains(&status) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("HTTP proxy rejected CONNECT with status {status}"),
                ));
            }
            return Ok(bytes.split_off(consumed));
        }
    }

    fn authority(destination: SocketAddr) -> String {
        match destination {
            SocketAddr::V4(address) => address.to_string(),
            SocketAddr::V6(address) => format!("[{}]:{}", address.ip(), address.port()),
        }
    }
}
