use super::protocol::{
    AUDIT_PROTOCOL_VERSION, AuditEventRequest, AuditRequest, AuditResponse, MAX_AUDIT_FRAME_SIZE,
    decode_request, decode_response, encode_request, encode_response, frame_length,
};
use super::{AuditClient, AuditController};
use crate::callback::{
    CommandContext, FileAccessMode, FileContext, FileOpenMode, ProcessContext, ProcessOperation,
};
use crate::callback::{Decision, Event, EventType};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn process_request() -> AuditEventRequest {
    AuditEventRequest::Process {
        trace_id: "trace-root, trace-child".to_string(),
        process: ProcessContext {
            pid: 42,
            ppid: 1,
            executable: "/bin/bash".to_string(),
        },
        command: CommandContext {
            executable: "/usr/bin/curl".to_string(),
            arguments: vec!["curl".to_string()],
            current_dir: "/tmp".to_string(),
            operation: ProcessOperation::PosixSpawn,
        },
    }
}

fn file_request() -> AuditEventRequest {
    AuditEventRequest::File {
        trace_id: "trace-root, trace-child".to_string(),
        process: ProcessContext {
            pid: 42,
            ppid: 1,
            executable: "/bin/bash".to_string(),
        },
        operation: super::protocol::FileOperation::Open,
        file: FileContext {
            path: "/tmp/data.json".to_string(),
            mode: FileOpenMode {
                access: FileAccessMode::ReadWrite,
                create: true,
                truncate: false,
                append: true,
                exclusive: false,
            },
        },
    }
}

#[test]
fn audit_protocol_round_trips_requests_and_responses() {
    let encoded = encode_request("token", process_request()).unwrap();
    let length = frame_length(encoded[..4].try_into().unwrap()).unwrap();
    let request = decode_request(&encoded[4..4 + length]).unwrap();

    assert_eq!(request.version, AUDIT_PROTOCOL_VERSION);
    assert_eq!(request.token, "token");
    assert_eq!(request.request_id.len(), 32);
    assert_eq!(request.event, process_request());

    let encoded = encode_response(&AuditResponse::Accepted).unwrap();
    let length = frame_length(encoded[..4].try_into().unwrap()).unwrap();
    assert_eq!(
        decode_response(&encoded[4..4 + length]).unwrap(),
        AuditResponse::Accepted
    );
}

#[test]
fn audit_protocol_rejects_empty_tokens_and_invalid_frames() {
    assert!(encode_request("", process_request()).is_err());
    assert!(frame_length(0_u32.to_be_bytes()).is_err());
    assert!(frame_length(((MAX_AUDIT_FRAME_SIZE + 1) as u32).to_be_bytes()).is_err());
    assert!(decode_response(b"not-json").is_err());
    assert!(decode_response(&vec![b'x'; MAX_AUDIT_FRAME_SIZE + 1]).is_err());

    let unsupported = serde_json::to_vec(&AuditRequest {
        version: AUDIT_PROTOCOL_VERSION + 1,
        token: "token".to_string(),
        request_id: "0".repeat(32),
        event: process_request(),
    })
    .unwrap();
    assert!(decode_request(&unsupported).is_err());

    let empty_token = serde_json::to_vec(&AuditRequest {
        version: AUDIT_PROTOCOL_VERSION,
        token: String::new(),
        request_id: "0".repeat(32),
        event: process_request(),
    })
    .unwrap();
    assert!(decode_request(&empty_token).is_err());

    let invalid_request_id = serde_json::to_vec(&AuditRequest {
        version: AUDIT_PROTOCOL_VERSION,
        token: "token".to_string(),
        request_id: "invalid".to_string(),
        event: process_request(),
    })
    .unwrap();
    assert!(decode_request(&invalid_request_id).is_err());

    let mut oversized = process_request();
    let AuditEventRequest::Process { command, .. } = &mut oversized else {
        unreachable!();
    };
    command.arguments = vec!["x".repeat(MAX_AUDIT_FRAME_SIZE)];
    assert!(encode_request("token", oversized).is_err());
}

#[tokio::test]
async fn audit_controller_enriches_hook_events_and_ignores_audit_decisions() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event: Event| {
            events.lock().unwrap().push(event);
            std::future::ready(Decision::Deny {
                reason: Some("audit events are not policy decisions".to_string()),
            })
        }
    };
    let controller = AuditController::start(
        "sandbox-1".to_string(),
        "run-1".to_string(),
        callback,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    let client = AuditClient::new(controller.runtime().control(), controller.runtime().token());

    tokio::task::spawn_blocking(move || {
        client.publish(process_request())?;
        client.publish(file_request())
    })
    .await
    .unwrap()
    .unwrap();

    {
        let events = events.lock().unwrap();
        let Event::Process(event) = &events[0] else {
            panic!("expected process event");
        };
        assert_eq!(event.event_type, EventType::ProcessExecAttempt);
        assert_eq!(event.sandbox_id, "sandbox-1");
        assert_eq!(event.run_id, "run-1");
        assert_eq!(event.trace_id, "trace-root, trace-child");
        let Event::File(event) = &events[1] else {
            panic!("expected file event");
        };
        assert_eq!(event.event_type, EventType::FilesystemOpen);
        assert_eq!(event.file.path, "/tmp/data.json");
        assert_eq!(event.file.mode.access, FileAccessMode::ReadWrite);
    }
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn audit_controller_reports_service_failure() {
    let mut controller = AuditController::start(
        "sandbox-1".to_string(),
        "run-1".to_string(),
        |_| std::future::ready(Decision::Allow),
        Duration::from_secs(1),
    )
    .await
    .unwrap();

    controller.abort_server_for_test();

    let error = controller.wait_failure().await;
    assert!(format!("{error:#}").contains("injected audit controller failure"));
}

#[tokio::test]
async fn audit_controller_propagates_service_failure_during_shutdown() {
    let mut controller = AuditController::start(
        "sandbox-1".to_string(),
        "run-1".to_string(),
        |_| std::future::ready(Decision::Allow),
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    controller.abort_server_for_test();

    let error = controller.shutdown().await.unwrap_err();
    assert!(format!("{error:#}").contains("injected audit controller failure"));
}

#[tokio::test]
async fn audit_controller_rejects_invalid_authentication_and_trace_context() {
    let controller = AuditController::start(
        "sandbox-1".to_string(),
        "run-1".to_string(),
        |_| std::future::ready(Decision::Allow),
        Duration::from_secs(1),
    )
    .await
    .unwrap();

    let client = AuditClient::new(controller.runtime().control(), "wrong-token");
    let error = tokio::task::spawn_blocking(move || client.publish(process_request()))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.errno(), libc::EACCES);
    assert_eq!(error.to_string(), "invalid audit token");

    let client = AuditClient::new(controller.runtime().control(), controller.runtime().token());
    let mut request = process_request();
    let AuditEventRequest::Process { trace_id, .. } = &mut request else {
        unreachable!();
    };
    trace_id.clear();
    let error = tokio::task::spawn_blocking(move || client.publish(request))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.errno(), libc::EINVAL);
    assert!(error.to_string().contains("invalid audit trace id"));

    controller.shutdown().await.unwrap();
}
