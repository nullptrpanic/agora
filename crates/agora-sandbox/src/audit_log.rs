use agora_core::logger::{self, LoggerEntry};
use agora_sandbox::callback::{
    Callback, Decision, Event, EventType, FileOpenMode, ProcessOperation,
};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub(super) struct JsonCallback;

impl JsonCallback {
    pub(super) fn new() -> Self {
        Self
    }
}

impl Callback for JsonCallback {
    fn on_event(&self, event: Event) -> impl Future<Output = Decision> + Send {
        if let Some(record) = record(&event) {
            logger::info!(
                entry = LoggerEntry::new().with_entry("audit", record),
                "sandbox audit event"
            );
        }
        std::future::ready(Decision::Allow)
    }
}

pub(super) fn record(event: &Event) -> Option<AuditRecord> {
    match event {
        Event::Network(event) if event.event_type == EventType::NetworkConnectAttempt => {
            event.network.as_ref().map(|network| AuditRecord::Network {
                access_time: event.occurred_at.clone(),
                trace_id: event.trace_id.clone(),
                pid: event.process.pid,
                destination_ip: network.destination_ip,
                destination_port: network.destination_port,
                target_host: network.target.as_deref().map(|target| target.host.clone()),
                target_port: network.target.as_deref().map(|target| target.port),
                domain: network.domain.clone(),
            })
        }
        Event::Process(event) if event.event_type == EventType::ProcessExecAttempt => {
            Some(AuditRecord::Process {
                access_time: event.occurred_at.clone(),
                trace_id: event.trace_id.clone(),
                pid: event.process.pid,
                ppid: event.process.ppid,
                process_executable: event.process.executable.clone(),
                executable: event.command.executable.clone(),
                arguments: event.command.arguments.clone(),
                current_dir: event.command.current_dir.clone(),
                operation: event.command.operation,
            })
        }
        Event::File(event)
            if matches!(
                event.event_type,
                EventType::FilesystemOpen | EventType::FilesystemClose
            ) =>
        {
            Some(AuditRecord::Filesystem {
                access_time: event.occurred_at.clone(),
                trace_id: event.trace_id.clone(),
                pid: event.process.pid,
                operation: match event.event_type {
                    EventType::FilesystemOpen => FileOperation::Open,
                    EventType::FilesystemClose => FileOperation::Close,
                    _ => unreachable!(),
                },
                path: event.file.path.clone(),
                mode: event.file.mode,
            })
        }
        _ => None,
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum AuditRecord {
    Network {
        access_time: String,
        trace_id: String,
        pid: u32,
        destination_ip: IpAddr,
        destination_port: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_host: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_port: Option<u16>,
        domain: Option<String>,
    },
    Process {
        access_time: String,
        trace_id: String,
        pid: u32,
        ppid: u32,
        process_executable: String,
        executable: String,
        arguments: Vec<String>,
        current_dir: String,
        operation: ProcessOperation,
    },
    Filesystem {
        access_time: String,
        trace_id: String,
        pid: u32,
        operation: FileOperation,
        path: String,
        mode: FileOpenMode,
    },
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FileOperation {
    Open,
    Close,
}
