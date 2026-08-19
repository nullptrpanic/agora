use super::audit_log::record as audit_record;
use super::{
    JsonCallback, exit_status_code, open_log, parse_command, shutdown_signals, signal_exit_code,
};
use agora_core::lifecycle::shutdown::ShutdownGuard;
use agora_sandbox::callback::{
    Callback, CommandContext, Decision, EVENT_SCHEMA_VERSION, Event, EventResult, EventStatus,
    EventType, FileAccessMode, FileContext, FileEvent, FileOpenMode, NetworkContext, NetworkEvent,
    NetworkProtocol, NetworkTarget, ProcessContext, ProcessEvent, ProcessOperation, Subsystem,
};
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use uuid::Uuid;

fn event(event_type: EventType, connection_id: Option<&str>, network: bool) -> NetworkEvent {
    NetworkEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: "event".to_string(),
        occurred_at: "2026-07-29T12:00:00Z".to_string(),
        subsystem: Subsystem::Network,
        event_type,
        sandbox_id: "sandbox".to_string(),
        run_id: "run".to_string(),
        trace_id: "trace-root".to_string(),
        connection_id: connection_id.map(ToString::to_string),
        sequence: Some(0),
        process: ProcessContext {
            pid: 42,
            ppid: 1,
            executable: "/usr/bin/curl".to_string(),
        },
        network: network.then_some(NetworkContext {
            protocol: NetworkProtocol::Tcp,
            destination_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)),
            destination_port: 443,
            target: None,
            http_host: None,
            tls_sni: None,
            domain: Some("example.com".to_string()),
            domain_source: None,
        }),
        tls: None,
        decision: None,
        result: EventResult {
            status: EventStatus::Started,
            error_code: None,
            error_message: None,
        },
        metrics: None,
    }
}

fn process_event() -> ProcessEvent {
    ProcessEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: "process-event".to_string(),
        occurred_at: "2026-07-29T12:00:01Z".to_string(),
        subsystem: Subsystem::Process,
        event_type: EventType::ProcessExecAttempt,
        sandbox_id: "sandbox".to_string(),
        run_id: "run".to_string(),
        trace_id: "trace-root, trace-child".to_string(),
        process: ProcessContext {
            pid: 43,
            ppid: 42,
            executable: "/bin/bash".to_string(),
        },
        command: CommandContext {
            executable: "/usr/bin/curl".to_string(),
            arguments: vec!["curl".to_string(), "https://example.com".to_string()],
            current_dir: "/tmp".to_string(),
            operation: ProcessOperation::PosixSpawn,
        },
        result: EventResult {
            status: EventStatus::Started,
            error_code: None,
            error_message: None,
        },
    }
}

fn file_event(event_type: EventType) -> FileEvent {
    FileEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: "file-event".to_string(),
        occurred_at: "2026-07-29T12:00:02Z".to_string(),
        subsystem: Subsystem::Filesystem,
        event_type,
        sandbox_id: "sandbox".to_string(),
        run_id: "run".to_string(),
        trace_id: "trace-root, trace-child".to_string(),
        process: ProcessContext {
            pid: 43,
            ppid: 42,
            executable: "/bin/bash".to_string(),
        },
        file: FileContext {
            path: "/Users/example/project/input.txt".to_string(),
            mode: FileOpenMode {
                access: FileAccessMode::ReadWrite,
                create: true,
                truncate: false,
                append: true,
                exclusive: false,
            },
        },
        result: EventResult {
            status: EventStatus::Started,
            error_code: None,
            error_message: None,
        },
    }
}

#[test]
fn command_parser_rejects_empty_input_and_preserves_quoted_arguments() {
    assert!(
        parse_command(" ")
            .unwrap_err()
            .to_string()
            .contains("contain a program")
    );
    let command = parse_command("/bin/echo 'hello world'").unwrap();
    let debug = format!("{command:?}");
    assert!(debug.contains("/bin/echo"));
    assert!(debug.contains("hello world"));
}

#[test]
fn audit_records_only_network_attempts_with_destination_details() {
    assert!(
        audit_record(&Event::Network(event(
            EventType::NetworkConnectAttempt,
            None,
            false,
        )))
        .is_none()
    );
    assert!(
        audit_record(&Event::Network(event(
            EventType::NetworkConnectDenied,
            Some("connection"),
            true,
        )))
        .is_none()
    );
    let record = audit_record(&Event::Network(event(
        EventType::NetworkConnectAttempt,
        Some("connection"),
        true,
    )))
    .unwrap();
    assert!(
        audit_record(&Event::Network(event(
            EventType::NetworkConnectFailed,
            Some("connection"),
            true,
        )))
        .is_none()
    );

    let record = serde_json::to_value(record).unwrap();
    assert_eq!(record["type"], "network");
    assert_eq!(record["domain"], "example.com");
    assert_eq!(record["destination_ip"], "203.0.113.10");
}

#[test]
fn audit_record_keeps_http_connect_target_separate_from_proxy_destination() {
    let mut event = event(EventType::NetworkConnectAttempt, Some("connection"), true);
    let network = event.network.as_mut().unwrap();
    network.destination_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    network.destination_port = 1087;
    network.target = Some(Box::new(NetworkTarget {
        host: "chatgpt.com".to_string(),
        port: 443,
    }));

    let record = serde_json::to_value(audit_record(&Event::Network(event)).unwrap()).unwrap();

    assert_eq!(record["destination_ip"], "127.0.0.1");
    assert_eq!(record["destination_port"], 1087);
    assert_eq!(record["target_host"], "chatgpt.com");
    assert_eq!(record["target_port"], 443);
}

#[test]
fn audit_records_process_network_and_filesystem_events() {
    let records = [
        Event::Process(process_event()),
        Event::File(file_event(EventType::FilesystemOpen)),
        Event::File(file_event(EventType::FilesystemClose)),
        Event::Network(event(
            EventType::NetworkConnectAttempt,
            Some("connection"),
            true,
        )),
    ]
    .iter()
    .map(|event| serde_json::to_value(audit_record(event).unwrap()).unwrap())
    .collect::<Vec<_>>();
    assert_eq!(records[0]["type"], "process");
    assert_eq!(records[0]["executable"], "/usr/bin/curl");
    assert_eq!(records[0]["arguments"][0], "curl");
    assert_eq!(records[0]["arguments"][1], "https://example.com");
    assert_eq!(records[0]["trace_id"], "trace-root, trace-child");
    assert_eq!(records[1]["type"], "filesystem");
    assert_eq!(records[1]["operation"], "open");
    assert_eq!(records[1]["path"], "/Users/example/project/input.txt");
    assert_eq!(records[1]["mode"]["access"], "read_write");
    assert_eq!(records[1]["mode"]["create"], true);
    assert_eq!(records[1]["mode"]["append"], true);
    assert_eq!(records[1]["trace_id"], "trace-root, trace-child");
    assert_eq!(records[2]["type"], "filesystem");
    assert_eq!(records[2]["operation"], "close");
    assert_eq!(records[3]["type"], "network");
    assert_eq!(records[3]["trace_id"], "trace-root");
}

#[test]
fn open_log_reports_an_unusable_output_directory() {
    let root = std::env::temp_dir().join(format!("agora-log-error-{}", Uuid::new_v4()));
    let blocked_parent = root.join("blocked");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&blocked_parent, b"not a directory").unwrap();

    let error = open_log(&blocked_parent.join("sandbox.jsonl"))
        .expect_err("a file cannot be used as a log directory");
    assert!(error.to_string().contains("failed to create log directory"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn open_log_creates_new_files_with_private_permissions() {
    let root = std::env::temp_dir().join(format!("agora-log-mode-{}", Uuid::new_v4()));
    let path = root.join("sandbox.jsonl");

    let output = open_log(&path).unwrap();

    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(output);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn open_log_preserves_existing_file_permissions() {
    let root = std::env::temp_dir().join(format!("agora-log-existing-{}", Uuid::new_v4()));
    let path = root.join("sandbox.jsonl");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

    let output = open_log(&path).unwrap();

    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    drop(output);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn open_log_rejects_a_symlink_without_touching_its_target() {
    let root = tempfile::tempdir().unwrap();
    let external = root.path().join("external.jsonl");
    let path = root.path().join("sandbox.jsonl");
    std::fs::write(&external, b"external").unwrap();
    std::os::unix::fs::symlink(&external, &path).unwrap();

    let accepted = open_log(&path).is_ok();

    assert!(!accepted, "a symbolic-link log file was accepted");
    assert_eq!(std::fs::read(external).unwrap(), b"external");
}

#[test]
fn open_log_rejects_a_symlinked_output_directory() {
    let root = tempfile::tempdir().unwrap();
    let external = root.path().join("external");
    let logs = root.path().join("logs");
    std::fs::create_dir(&external).unwrap();
    std::os::unix::fs::symlink(&external, &logs).unwrap();

    let accepted = open_log(&logs.join("sandbox.jsonl")).is_ok();

    assert!(!accepted, "a symbolic-link log directory was accepted");
    assert_eq!(std::fs::read_dir(external).unwrap().count(), 0);
}

#[tokio::test]
async fn exit_codes_and_shutdown_signals_are_stable() {
    let status = Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .status()
        .unwrap();
    assert_eq!(exit_status_code(status), 7);
    assert_eq!(signal_exit_code(15), 143);
    assert_eq!(signal_exit_code(i32::MAX), u8::MAX);
    shutdown_signals(&ShutdownGuard::get()).unwrap();
}

#[tokio::test]
async fn json_callback_allows_audit_events() {
    let callback = JsonCallback::new();

    assert!(matches!(
        callback.on_event(Event::Process(process_event())).await,
        Decision::Allow
    ));
}

#[tokio::test]
async fn json_callback_allows_unrecorded_events() {
    let callback = JsonCallback::new();

    assert!(matches!(
        callback
            .on_event(Event::Network(event(
                EventType::NetworkConnectEstablished,
                Some("connection"),
                true,
            )))
            .await,
        Decision::Allow
    ));
}
