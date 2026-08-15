use agora_sandbox::callback::{
    BasicAuth, Callback, CommandContext, Decision, DomainSource, EVENT_SCHEMA_VERSION, Event,
    EventMetrics, EventResult, EventStatus, EventType, FileAccessMode, FileContext, FileEvent,
    FileOpenMode, HttpProxy, NetworkContext, NetworkEvent, NetworkProtocol, NetworkTarget,
    NoopCallback, ProcessContext, ProcessEvent, ProcessOperation, Proxy, Redact, Subsystem,
};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

fn network_event() -> NetworkEvent {
    NetworkEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: "event-1".to_string(),
        occurred_at: "2026-07-23T12:34:56.789Z".to_string(),
        subsystem: Subsystem::Network,
        event_type: EventType::NetworkConnectAttempt,
        sandbox_id: "sandbox-1".to_string(),
        run_id: "run-1".to_string(),
        trace_id: "trace-root".to_string(),
        connection_id: Some("connection-1".to_string()),
        sequence: Some(0),
        process: ProcessContext {
            pid: 101,
            ppid: 100,
            executable: "/usr/bin/curl".to_string(),
        },
        network: Some(NetworkContext {
            protocol: NetworkProtocol::Tcp,
            destination_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)),
            destination_port: 443,
            target: Some(Box::new(NetworkTarget {
                host: "example.com".to_string(),
                port: 443,
            })),
            http_host: Some("example.com".to_string()),
            tls_sni: None,
            domain: Some("example.com".to_string()),
            domain_source: Some(DomainSource::HttpHost),
        }),
        tls: None,
        decision: None,
        result: EventResult {
            status: EventStatus::Started,
            error_code: None,
            error_message: None,
        },
        metrics: Some(EventMetrics {
            bytes_sent: 0,
            bytes_received: 0,
            duration_ms: 0,
        }),
    }
}

fn process_event() -> ProcessEvent {
    ProcessEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: "event-2".to_string(),
        occurred_at: "2026-07-23T12:34:56.789Z".to_string(),
        subsystem: Subsystem::Process,
        event_type: EventType::ProcessExecAttempt,
        sandbox_id: "sandbox-1".to_string(),
        run_id: "run-1".to_string(),
        trace_id: "trace-root, trace-child".to_string(),
        process: ProcessContext {
            pid: 101,
            ppid: 100,
            executable: "/bin/bash".to_string(),
        },
        command: CommandContext {
            executable: "/usr/bin/curl".to_string(),
            arguments: vec!["curl".to_string(), "https://example.com".to_string()],
            current_dir: "/tmp".to_string(),
            operation: ProcessOperation::Execve,
        },
        result: EventResult {
            status: EventStatus::Started,
            error_code: None,
            error_message: None,
        },
    }
}

fn file_event() -> FileEvent {
    FileEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: "event-3".to_string(),
        occurred_at: "2026-07-23T12:34:56.789Z".to_string(),
        subsystem: Subsystem::Filesystem,
        event_type: EventType::FilesystemOpen,
        sandbox_id: "sandbox-1".to_string(),
        run_id: "run-1".to_string(),
        trace_id: "trace-root, trace-child".to_string(),
        process: ProcessContext {
            pid: 101,
            ppid: 100,
            executable: "/usr/bin/curl".to_string(),
        },
        file: FileContext {
            path: "/tmp/output.json".to_string(),
            mode: FileOpenMode {
                access: FileAccessMode::ReadWrite,
                create: true,
                truncate: true,
                append: false,
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
fn callback_event_uses_stable_versioned_json_fields() {
    let event = Event::Network(network_event());
    let value = serde_json::to_value(event.redacted()).unwrap();

    assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
    assert_eq!(value["subsystem"], "network");
    assert_eq!(value["event_type"], "network.connect.attempt");
    assert_eq!(value["network"]["protocol"], "tcp");
    assert_eq!(value["network"]["destination_ip"], "203.0.113.10");
    assert_eq!(value["network"]["target_host"], "example.com");
    assert_eq!(value["network"]["target_port"], 443);
    assert!(value["network"].get("source_ip").is_none());
    assert!(value["network"].get("source_port").is_none());
    assert_eq!(value["network"]["domain_source"], "http_host");
    assert!(value["decision"].is_null());
    assert_eq!(value["result"]["status"], "started");
}

#[test]
fn process_event_preserves_arguments_at_the_redacted_event_boundary() {
    let event = Event::Process(process_event());
    let value = serde_json::to_value(event.redacted()).unwrap();

    assert!(event.as_network().is_none());
    assert!(event.clone().into_network().is_none());
    assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
    assert_eq!(value["subsystem"], "process");
    assert_eq!(value["event_type"], "process.exec.attempt");
    assert_eq!(value["trace_id"], "trace-root, trace-child");
    assert_eq!(value["command"]["executable"], "/usr/bin/curl");
    assert_eq!(value["command"]["operation"], "execve");
    assert_eq!(value["command"]["arguments"][0], "curl");
    assert_eq!(value["command"]["arguments"][1], "https://example.com");
}

#[test]
fn file_event_preserves_logical_path_mode_and_trace_context() {
    let value = serde_json::to_value(Event::File(file_event()).redacted()).unwrap();

    assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
    assert_eq!(value["subsystem"], "filesystem");
    assert_eq!(value["event_type"], "filesystem.open");
    assert_eq!(value["trace_id"], "trace-root, trace-child");
    assert_eq!(value["file"]["path"], "/tmp/output.json");
    assert_eq!(value["file"]["mode"]["access"], "read_write");
    assert_eq!(value["file"]["mode"]["create"], true);
    assert_eq!(value["file"]["mode"]["truncate"], true);
}

#[test]
fn proxy_decision_is_redacted_before_serialization_and_debug_output() {
    let mut event = network_event();
    event.decision = Some(Decision::Proxy {
        proxy: Proxy::Http(HttpProxy {
            address: "proxy.example:8080".to_string(),
            basic_auth: Some(BasicAuth {
                username: "alice".to_string(),
                password: "secret-password".to_string(),
            }),
        }),
    });

    let json = serde_json::to_string(&event.redacted()).unwrap();
    assert!(json.contains("proxy.example:8080"));
    assert!(json.contains("alice"));
    assert!(json.contains("[redacted]"));
    assert!(!json.contains("secret-password"));

    let debug = format!("{:?}", event.decision.as_ref().unwrap());
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains("secret-password"));
}

#[test]
fn allow_and_deny_decisions_have_stable_redacted_json() {
    let allow = serde_json::to_value(Decision::Allow.redacted()).unwrap();
    assert_eq!(allow, serde_json::json!({ "action": "allow" }));

    let deny = Decision::Deny {
        reason: Some("blocked by policy".to_string()),
    };
    let deny = serde_json::to_value(deny.redacted()).unwrap();
    assert_eq!(
        deny,
        serde_json::json!({
            "action": "deny",
            "reason": "blocked by policy",
        })
    );

    let proxy = Decision::Proxy {
        proxy: Proxy::Http(HttpProxy {
            address: "proxy.example:8080".to_string(),
            basic_auth: None,
        }),
    };
    let proxy = serde_json::to_value(proxy.redacted()).unwrap();
    assert!(proxy["proxy"]["basic_auth"].is_null());
}

#[tokio::test]
async fn closure_callback_receives_an_owned_event() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let received = Arc::clone(&received);
        move |event: Event| {
            received.lock().unwrap().push(event);
            std::future::ready(Decision::Allow)
        }
    };

    assert_eq!(
        callback.on_event(Event::Network(network_event())).await,
        Decision::Allow
    );

    let events = received.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].as_network().unwrap().event_id, "event-1");
}

#[tokio::test]
async fn noop_callback_allows_events_without_side_effects() {
    assert_eq!(
        NoopCallback.on_event(Event::Network(network_event())).await,
        Decision::Allow
    );
}
