use super::{
    ConnectRequest, HookOperation, PROTOCOL_VERSION, ProcessIdentity, encode_connect_request,
    parse_connect_request_prefix,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn connect_request() -> ConnectRequest {
    ConnectRequest {
        protocol_version: PROTOCOL_VERSION,
        token: "token-1".to_string(),
        connection_id: "connection-1".to_string(),
        destination: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 443),
        process: ProcessIdentity {
            pid: 101,
            ppid: 100,
            executable: "/usr/bin/curl".to_string(),
        },
        trace_id: "trace-root, trace-curl".to_string(),
        operation: HookOperation::Connect,
    }
}

#[test]
fn connect_request_prefix_preserves_trailing_tunnel_bytes() {
    let request = connect_request();
    let mut encoded = encode_connect_request(&request).unwrap();
    encoded.extend_from_slice(b"hello");

    let (parsed, consumed) = parse_connect_request_prefix(&encoded).unwrap().unwrap();

    assert_eq!(parsed, request);
    assert_eq!(&encoded[consumed..], b"hello");
}

fn split_request(message: &[u8]) -> (&[u8], &[u8]) {
    let boundary = message
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
        .unwrap();
    message.split_at(boundary)
}

#[test]
fn connect_request_round_trips_through_http_headers() {
    let request = connect_request();
    let encoded = encode_connect_request(&request).unwrap();
    let (head, body) = split_request(&encoded);

    assert!(head.starts_with(b"CONNECT 203.0.113.10:443 HTTP/1.1\r\n"));
    assert!(
        !head
            .windows(b"Agora-Mode".len())
            .any(|window| window == b"Agora-Mode")
    );
    assert!(
        !head
            .windows(b"Agora-Run-Id".len())
            .any(|window| window == b"Agora-Run-Id")
    );
    assert!(
        !head
            .windows(b"Agora-Sandbox-Id".len())
            .any(|window| window == b"Agora-Sandbox-Id")
    );
    assert!(
        head.windows(b"Proxy-Authorization: Bearer token-1".len())
            .any(|window| window == b"Proxy-Authorization: Bearer token-1")
    );
    assert!(
        head.windows(b"Agora-Trace-Id: trace-root, trace-curl".len())
            .any(|window| window == b"Agora-Trace-Id: trace-root, trace-curl")
    );
    assert!(body.is_empty());
    let (parsed, consumed) = parse_connect_request_prefix(head).unwrap().unwrap();
    assert_eq!(consumed, head.len());
    assert_eq!(parsed, request);
}

#[test]
fn basic_authorization_is_rejected() {
    let request = encode_connect_request(&connect_request()).unwrap();
    let request = String::from_utf8(request)
        .unwrap()
        .replace("Bearer token-1", "Basic token-1");
    let (head, body) = split_request(request.as_bytes());

    assert!(body.is_empty());
    let error = parse_connect_request_prefix(head).unwrap_err();

    assert!(error.to_string().contains("Proxy-Authorization"));
}

#[test]
fn connect_host_must_match_the_target() {
    let request = encode_connect_request(&connect_request()).unwrap();
    let request = String::from_utf8(request)
        .unwrap()
        .replace("Host: 203.0.113.10:443", "Host: 203.0.113.11:443");
    let (head, body) = split_request(request.as_bytes());

    assert!(body.is_empty());
    let error = parse_connect_request_prefix(head).unwrap_err();

    assert!(error.to_string().contains("Host does not match"));
}

#[test]
fn connectx_request_round_trips_and_becomes_a_route_registration() {
    let mut request = connect_request();
    request.operation = HookOperation::Connectx;
    request.process.executable = "/tmp/客户端".to_string();

    let encoded = encode_connect_request(&request).unwrap();
    let (parsed, _) = parse_connect_request_prefix(&encoded).unwrap().unwrap();
    assert_eq!(parsed, request);

    let registration = parsed.into_registration();
    assert_eq!(registration.connection_id, "connection-1");
    assert_eq!(registration.destination, request.destination);
    assert_eq!(registration.process, request.process);
    assert_eq!(registration.trace_id, request.trace_id);
    assert_eq!(registration.operation, HookOperation::Connectx);
}

#[test]
fn encoder_rejects_values_that_cannot_be_safe_http_headers() {
    let mut request = connect_request();
    request.token.clear();
    assert_eq!(
        encode_connect_request(&request).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );

    request.token = "bad token".to_string();
    assert!(encode_connect_request(&request).is_err());
    request.token = "token+/=".to_string();
    request.connection_id = "line\nbreak".to_string();
    assert!(encode_connect_request(&request).is_err());

    request.connection_id = "connection-1".to_string();
    request.process.executable = "x".repeat(super::MAX_FRAME_SIZE);
    assert_eq!(
        encode_connect_request(&request).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn parser_rejects_invalid_request_lines_and_body_headers() {
    assert!(
        parse_connect_request_prefix(b"CONNECT 127.0.0.1:80 HTTP/1.1\r\n")
            .unwrap()
            .is_none()
    );

    let valid = String::from_utf8(encode_connect_request(&connect_request()).unwrap()).unwrap();
    let cases = [
        (valid.replacen("HTTP/1.1", "HTTP/1.0", 1), "HTTP/1.1"),
        (
            valid.replacen("CONNECT", "GET", 1),
            "unsupported proxy request",
        ),
        (
            valid.replacen("203.0.113.10:443", "example.com:443", 2),
            "CONNECT target",
        ),
        (
            valid.replace("\r\n\r\n", "\r\nContent-Length: 1\r\n\r\n"),
            "bodies are not supported",
        ),
        (
            valid.replace("\r\n\r\n", "\r\nTransfer-Encoding: chunked\r\n\r\n"),
            "Transfer-Encoding",
        ),
        (
            valid.replace("\r\n\r\n", "\r\nContent-Length: invalid\r\n\r\n"),
            "invalid Content-Length",
        ),
        (
            valid.replace(
                "\r\n\r\n",
                &format!("\r\nContent-Length: {}\r\n\r\n", super::MAX_FRAME_SIZE + 1),
            ),
            "request body exceeds",
        ),
    ];

    for (message, expected) in cases {
        let error = parse_connect_request_prefix(message.as_bytes()).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }

    let error = parse_connect_request_prefix(b"not http\r\n\r\n").unwrap_err();
    assert!(error.to_string().contains("invalid HTTP request"));
}

#[test]
fn parser_rejects_missing_duplicate_and_malformed_agora_headers() {
    let valid = String::from_utf8(encode_connect_request(&connect_request()).unwrap()).unwrap();
    let cases = [
        (
            valid.replace("Host: 203.0.113.10:443\r\n", ""),
            "missing Host",
        ),
        (
            valid.replace(
                "Host: 203.0.113.10:443\r\n",
                "Host: 203.0.113.10:443\r\nHost: 203.0.113.10:443\r\n",
            ),
            "duplicate Host",
        ),
        (
            valid.replace("Bearer token-1", "Bearer"),
            "Proxy-Authorization",
        ),
        (
            valid.replace("Agora-Operation: connect", "Agora-Operation: unknown"),
            "Agora-Operation",
        ),
        (
            valid.replace(
                &format!("Agora-Version: {PROTOCOL_VERSION}"),
                "Agora-Version: invalid",
            ),
            "Agora-Version",
        ),
        (
            valid.replace("Agora-Pid: 101", "Agora-Pid: invalid"),
            "Agora-Pid",
        ),
        (
            valid.replace("Agora-Ppid: 100", "Agora-Ppid: invalid"),
            "Agora-Ppid",
        ),
        (
            valid.replace("Agora-Executable-Hex: 2f", "Agora-Executable-Hex: f"),
            "executable encoding",
        ),
        (
            valid.replace("Agora-Executable-Hex: 2f", "Agora-Executable-Hex: gg"),
            "executable encoding",
        ),
        (
            valid.replace("Agora-Executable-Hex: 2f", "Agora-Executable-Hex: FF"),
            "executable encoding",
        ),
    ];

    for (message, expected) in cases {
        let error = parse_connect_request_prefix(message.as_bytes()).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }
}

#[test]
fn protocol_error_constructors_preserve_their_messages() {
    assert_eq!(super::ProtocolError::bad_request("bad").to_string(), "bad");
    assert_eq!(
        super::ProtocolError::unauthorized("unauthorized").to_string(),
        "unauthorized"
    );
    assert_eq!(
        super::ProtocolError::version_not_supported("version").to_string(),
        "version"
    );
}
