use crate::trace::{TRACE_ID_HEADER, TraceContext};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

pub const PROTOCOL_VERSION: u16 = 7;
pub const MAX_FRAME_SIZE: usize = 16 * 1024;
pub const MAX_HEADERS: usize = 32;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectRequest {
    pub protocol_version: u16,
    pub token: String,
    pub connection_id: String,
    pub destination: SocketAddr,
    pub process: ProcessIdentity,
    pub trace_id: String,
    pub operation: HookOperation,
}

impl ConnectRequest {
    pub fn into_registration(self) -> RouteRegistration {
        RouteRegistration {
            connection_id: self.connection_id,
            destination: self.destination,
            process: self.process,
            trace_id: self.trace_id,
            operation: self.operation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRegistration {
    pub connection_id: String,
    pub destination: SocketAddr,
    pub process: ProcessIdentity,
    pub trace_id: String,
    pub operation: HookOperation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub ppid: u32,
    pub executable: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOperation {
    Connect,
    Connectx,
}

impl HookOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Connectx => "connectx",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "connect" => Some(Self::Connect),
            "connectx" => Some(Self::Connectx),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError {
    message: String,
}

impl ProtocolError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(message)
    }

    pub fn version_not_supported(message: impl Into<String>) -> Self {
        Self::new(message)
    }

    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolError {}

pub fn encode_connect_request(request: &ConnectRequest) -> io::Result<Vec<u8>> {
    validate_token(&request.token)?;
    validate_header_value("connection id", &request.connection_id, false)?;
    let trace_id = TraceContext::parse(&request.trace_id)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .encode();

    let authority = request.destination.to_string();
    let executable = hex_encode(request.process.executable.as_bytes());
    let message = format!(
        "CONNECT {authority} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Proxy-Authorization: Bearer {}\r\n\
         Agora-Version: {}\r\n\
         Agora-Connection-Id: {}\r\n\
         Agora-Pid: {}\r\n\
         Agora-Ppid: {}\r\n\
         Agora-Operation: {}\r\n\
         {TRACE_ID_HEADER}: {}\r\n\
         Agora-Executable-Hex: {}\r\n\
         \r\n",
        request.token,
        request.protocol_version,
        request.connection_id,
        request.process.pid,
        request.process.ppid,
        request.operation.as_str(),
        trace_id,
        executable,
    );
    ensure_frame_size(message.len())?;
    Ok(message.into_bytes())
}

pub fn parse_connect_request_prefix(
    bytes: &[u8],
) -> Result<Option<(ConnectRequest, usize)>, ProtocolError> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers);
    let consumed = match parsed.parse(bytes) {
        Ok(httparse::Status::Complete(consumed)) => consumed,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(error) => {
            return Err(ProtocolError::bad_request(format!(
                "invalid HTTP request: {error}"
            )));
        }
    };
    if parsed.version != Some(1) {
        return Err(ProtocolError::bad_request(
            "proxy requests must use HTTP/1.1",
        ));
    }

    let (Some("CONNECT"), Some(target)) = (parsed.method, parsed.path) else {
        return Err(ProtocolError::new("unsupported proxy request"));
    };
    if request_body_length_from_headers(parsed.headers)? != 0 {
        return Err(ProtocolError::bad_request(
            "CONNECT request bodies are not supported",
        ));
    }
    let destination = target
        .parse::<SocketAddr>()
        .map_err(|_| ProtocolError::bad_request("CONNECT target must be an IP:port"))?;
    let host = required_header_string(parsed.headers, "Host")?
        .parse::<SocketAddr>()
        .map_err(|_| ProtocolError::bad_request("Host must be an IP:port"))?;
    if host != destination {
        return Err(ProtocolError::bad_request(
            "Host does not match CONNECT target",
        ));
    }
    let operation =
        HookOperation::parse(required_header_string(parsed.headers, "Agora-Operation")?)
            .ok_or_else(|| ProtocolError::bad_request("invalid Agora-Operation"))?;
    let executable = hex_decode(required_header_string(
        parsed.headers,
        "Agora-Executable-Hex",
    )?)?;
    let executable = String::from_utf8(executable)
        .map_err(|_| ProtocolError::bad_request("invalid executable encoding"))?;
    let trace_id = TraceContext::parse(required_header_string(parsed.headers, TRACE_ID_HEADER)?)
        .map_err(ProtocolError::bad_request)?;
    Ok(Some((
        ConnectRequest {
            protocol_version: required_header_string(parsed.headers, "Agora-Version")?
                .parse::<u16>()
                .map_err(|_| ProtocolError::bad_request("invalid Agora-Version"))?,
            token: bearer_token(parsed.headers)?.to_string(),
            connection_id: required_header_string(parsed.headers, "Agora-Connection-Id")?
                .to_string(),
            destination,
            process: ProcessIdentity {
                pid: parse_u32_header(parsed.headers, "Agora-Pid")?,
                ppid: parse_u32_header(parsed.headers, "Agora-Ppid")?,
                executable,
            },
            trace_id: trace_id.encode(),
            operation,
        },
        consumed,
    )))
}

fn request_body_length_from_headers(
    headers: &[httparse::Header<'_>],
) -> Result<usize, ProtocolError> {
    if optional_header(headers, "Transfer-Encoding")?.is_some() {
        return Err(ProtocolError::bad_request(
            "Transfer-Encoding is not supported",
        ));
    }
    let Some(value) = optional_header_string(headers, "Content-Length")? else {
        return Ok(0);
    };
    let length = value
        .parse::<usize>()
        .map_err(|_| ProtocolError::bad_request("invalid Content-Length"))?;
    if length > MAX_FRAME_SIZE {
        return Err(ProtocolError::bad_request(format!(
            "request body exceeds {MAX_FRAME_SIZE} bytes"
        )));
    }
    Ok(length)
}

fn bearer_token<'a>(headers: &[httparse::Header<'a>]) -> Result<&'a str, ProtocolError> {
    let credentials = required_header_string(headers, "Proxy-Authorization")?;
    let Some((scheme, token)) = credentials.split_once(' ') else {
        return Err(ProtocolError::unauthorized(
            "invalid Proxy-Authorization credentials",
        ));
    };
    if !scheme.eq_ignore_ascii_case("Bearer") || !is_token68(token) {
        return Err(ProtocolError::unauthorized(
            "invalid Proxy-Authorization credentials",
        ));
    }
    Ok(token)
}

fn parse_u32_header(headers: &[httparse::Header<'_>], name: &str) -> Result<u32, ProtocolError> {
    required_header_string(headers, name)?
        .parse::<u32>()
        .map_err(|_| ProtocolError::bad_request(format!("invalid {name}")))
}

fn required_header_string<'a>(
    headers: &[httparse::Header<'a>],
    name: &str,
) -> Result<&'a str, ProtocolError> {
    let value = optional_header(headers, name)?
        .ok_or_else(|| ProtocolError::bad_request(format!("missing {name} header")))?;
    std::str::from_utf8(value)
        .map_err(|_| ProtocolError::bad_request(format!("invalid {name} header")))
}

fn optional_header_string<'a>(
    headers: &[httparse::Header<'a>],
    name: &str,
) -> Result<Option<&'a str>, ProtocolError> {
    optional_header(headers, name)?
        .map(|value| {
            std::str::from_utf8(value)
                .map_err(|_| ProtocolError::bad_request(format!("invalid {name} header")))
        })
        .transpose()
}

fn optional_header<'a>(
    headers: &[httparse::Header<'a>],
    name: &str,
) -> Result<Option<&'a [u8]>, ProtocolError> {
    let mut matching = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name));
    let value = matching.next().map(|header| header.value);
    if matching.next().is_some() {
        return Err(ProtocolError::bad_request(format!(
            "duplicate {name} header"
        )));
    }
    Ok(value)
}

fn validate_token(token: &str) -> io::Result<()> {
    if is_token68(token) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid bearer token",
        ))
    }
}

fn is_token68(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
}

fn validate_header_value(name: &str, value: &str, allow_empty: bool) -> io::Result<()> {
    if (allow_empty || !value.is_empty())
        && value
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {name}"),
        ))
    }
}

fn ensure_frame_size(length: usize) -> io::Result<()> {
    if length <= MAX_FRAME_SIZE {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HTTP frame exceeds {MAX_FRAME_SIZE} bytes"),
        ))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn hex_decode(value: &str) -> Result<Vec<u8>, ProtocolError> {
    if !value.len().is_multiple_of(2) {
        return Err(ProtocolError::bad_request("invalid executable encoding"));
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, ProtocolError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(ProtocolError::bad_request("invalid executable encoding")),
    }
}

#[cfg(test)]
mod tests;
