use crate::callback::{CommandContext, FileContext, ProcessContext};
use serde::{Deserialize, Serialize};
use std::io;

pub(super) const AUDIT_PROTOCOL_VERSION: u16 = 4;
pub(super) const MAX_AUDIT_FRAME_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditRequest {
    pub(crate) version: u16,
    pub(crate) token: String,
    pub(crate) request_id: String,
    pub(crate) event: AuditEventRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AuditEventRequest {
    Ping,
    Process {
        trace_id: String,
        process: ProcessContext,
        command: CommandContext,
    },
    File {
        trace_id: String,
        process: ProcessContext,
        operation: FileOperation,
        file: FileContext,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileOperation {
    Open,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AuditResponse {
    Accepted,
    Error { errno: i32, message: String },
}

pub(crate) fn encode_request(token: &str, event: AuditEventRequest) -> io::Result<Vec<u8>> {
    if token.is_empty() {
        return Err(invalid_input("audit token is empty"));
    }
    encode(&AuditRequest {
        version: AUDIT_PROTOCOL_VERSION,
        token: token.to_string(),
        request_id: uuid::Uuid::new_v4().simple().to_string(),
        event,
    })
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(crate) fn encode_ping_request(token: &str) -> io::Result<Vec<u8>> {
    encode_request(token, AuditEventRequest::Ping)
}

pub(super) fn decode_request(frame: &[u8]) -> io::Result<AuditRequest> {
    let request: AuditRequest = decode(frame)?;
    if request.version != AUDIT_PROTOCOL_VERSION {
        return Err(invalid_data("unsupported audit protocol version"));
    }
    if request.token.is_empty() {
        return Err(invalid_data("audit token is empty"));
    }
    if request.request_id.len() != 32
        || !request
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_data("invalid audit request ID"));
    }
    Ok(request)
}

pub(super) fn encode_response(response: &AuditResponse) -> io::Result<Vec<u8>> {
    encode(response)
}

pub(crate) fn decode_response(frame: &[u8]) -> io::Result<AuditResponse> {
    decode(frame)
}

pub(crate) fn frame_length(prefix: [u8; 4]) -> io::Result<usize> {
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_AUDIT_FRAME_SIZE {
        return Err(invalid_data("invalid audit frame length"));
    }
    Ok(length)
}

fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if body.is_empty() || body.len() > MAX_AUDIT_FRAME_SIZE {
        return Err(invalid_input("audit frame is too large"));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn decode<T: for<'de> Deserialize<'de>>(frame: &[u8]) -> io::Result<T> {
    if frame.is_empty() || frame.len() > MAX_AUDIT_FRAME_SIZE {
        return Err(invalid_data("invalid audit frame length"));
    }
    serde_json::from_slice(frame).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
