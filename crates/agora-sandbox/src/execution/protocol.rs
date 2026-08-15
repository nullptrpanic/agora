use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

pub(crate) const EXECUTION_PROTOCOL_VERSION: u16 = 6;
pub(super) const MAX_EXECUTION_FRAME_SIZE: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PrepareRequest {
    pub(crate) token: String,
    pub(crate) executable: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExecutionRequest {
    Ping { token: String },
    Prepare(PrepareRequest),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrepareResponse {
    Accepted,
    Ready(PathBuf),
    Error { errno: i32, message: String },
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(crate) fn encode_ping_request(token: &str) -> io::Result<Vec<u8>> {
    encode_request(0, token, &[])
}

pub(crate) fn encode_prepare_request(token: &str, executable: &Path) -> io::Result<Vec<u8>> {
    let executable = executable.as_os_str().as_bytes();
    if executable.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path is empty",
        ));
    }
    encode_request(1, token, executable)
}

fn encode_request(operation: u8, token: &str, executable: &[u8]) -> io::Result<Vec<u8>> {
    if token.is_empty() || token.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid execution token length",
        ));
    }
    let executable_length = u32::try_from(executable.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "executable path is too long"))?;
    let mut body = Vec::with_capacity(9 + token.len() + executable.len());
    body.extend_from_slice(&EXECUTION_PROTOCOL_VERSION.to_be_bytes());
    body.push(operation);
    body.extend_from_slice(&(token.len() as u16).to_be_bytes());
    body.extend_from_slice(&executable_length.to_be_bytes());
    body.extend_from_slice(token.as_bytes());
    body.extend_from_slice(executable);
    encode_frame(body)
}

#[cfg(test)]
pub(crate) fn decode_prepare_request(frame: &[u8]) -> io::Result<PrepareRequest> {
    match decode_request(frame)? {
        ExecutionRequest::Prepare(request) => Ok(request),
        ExecutionRequest::Ping { .. } => Err(invalid_data("expected an execution prepare request")),
    }
}

pub(super) fn decode_request(frame: &[u8]) -> io::Result<ExecutionRequest> {
    if frame.len() < 9 {
        return Err(invalid_data("execution request is truncated"));
    }
    let version = u16::from_be_bytes([frame[0], frame[1]]);
    if version != EXECUTION_PROTOCOL_VERSION {
        return Err(invalid_data("unsupported execution protocol version"));
    }
    let operation = frame[2];
    let token_length = u16::from_be_bytes([frame[3], frame[4]]) as usize;
    let path_length = u32::from_be_bytes([frame[5], frame[6], frame[7], frame[8]]) as usize;
    let expected = 9_usize
        .checked_add(token_length)
        .and_then(|length| length.checked_add(path_length))
        .ok_or_else(|| invalid_data("execution request length overflow"))?;
    if expected != frame.len() || token_length == 0 {
        return Err(invalid_data("invalid execution request lengths"));
    }
    let token_end = 9 + token_length;
    let path_end = token_end + path_length;
    let token = std::str::from_utf8(&frame[9..token_end])
        .map_err(|_| invalid_data("execution token is not UTF-8"))?
        .to_string();
    match (operation, path_length) {
        (0, 0) => Ok(ExecutionRequest::Ping { token }),
        (1, 1..) => Ok(ExecutionRequest::Prepare(PrepareRequest {
            token,
            executable: PathBuf::from(OsString::from_vec(frame[token_end..path_end].to_vec())),
        })),
        _ => Err(invalid_data("invalid execution request operation")),
    }
}

pub(super) fn encode_prepare_response(response: &PrepareResponse) -> io::Result<Vec<u8>> {
    let (status, content) = match response {
        PrepareResponse::Accepted => (0_u8, Vec::new()),
        PrepareResponse::Ready(path) => (1_u8, path.as_os_str().as_bytes().to_vec()),
        PrepareResponse::Error { errno, message } => {
            if *errno <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid execution response errno",
                ));
            }
            let mut content = Vec::with_capacity(4 + message.len());
            content.extend_from_slice(&errno.to_be_bytes());
            content.extend_from_slice(message.as_bytes());
            (2_u8, content)
        }
    };
    let content_length = u32::try_from(content.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "response is too large"))?;
    let mut body = Vec::with_capacity(7 + content.len());
    body.extend_from_slice(&EXECUTION_PROTOCOL_VERSION.to_be_bytes());
    body.push(status);
    body.extend_from_slice(&content_length.to_be_bytes());
    body.extend_from_slice(&content);
    encode_frame(body)
}

pub(crate) fn decode_prepare_response(frame: &[u8]) -> io::Result<PrepareResponse> {
    if frame.len() < 7 {
        return Err(invalid_data("execution response is truncated"));
    }
    let version = u16::from_be_bytes([frame[0], frame[1]]);
    if version != EXECUTION_PROTOCOL_VERSION {
        return Err(invalid_data("unsupported execution protocol version"));
    }
    let content_length = u32::from_be_bytes([frame[3], frame[4], frame[5], frame[6]]) as usize;
    if 7_usize.checked_add(content_length) != Some(frame.len()) {
        return Err(invalid_data("invalid execution response length"));
    }
    match frame[2] {
        0 if content_length == 0 => Ok(PrepareResponse::Accepted),
        1 if content_length != 0 => Ok(PrepareResponse::Ready(PathBuf::from(OsString::from_vec(
            frame[7..].to_vec(),
        )))),
        2 if content_length >= 4 => {
            let errno = i32::from_be_bytes(frame[7..11].try_into().unwrap());
            if errno <= 0 {
                return Err(invalid_data("invalid execution response errno"));
            }
            Ok(PrepareResponse::Error {
                errno,
                message: std::str::from_utf8(&frame[11..])
                    .map_err(|_| invalid_data("execution error is not UTF-8"))?
                    .to_string(),
            })
        }
        2 => Err(invalid_data("execution error is truncated")),
        _ => Err(invalid_data("invalid execution response status")),
    }
}

pub(crate) fn frame_length(prefix: [u8; 4]) -> io::Result<usize> {
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_EXECUTION_FRAME_SIZE {
        return Err(invalid_data("invalid execution frame length"));
    }
    Ok(length)
}

fn encode_frame(body: Vec<u8>) -> io::Result<Vec<u8>> {
    if body.is_empty() || body.len() > MAX_EXECUTION_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid execution frame length",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
