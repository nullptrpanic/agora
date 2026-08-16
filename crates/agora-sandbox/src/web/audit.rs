use crate::audit_log::AuditRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

const MAX_LINE_BYTES: usize = 256 * 1024;
const MARKER_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TraceKind {
    Exec,
    FileOpen,
    FileClose,
    Network,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TraceEvent {
    pub(super) id: u64,
    pub(super) root_trace_id: String,
    pub(super) kind: TraceKind,
    pub(super) occurred_at: String,
    pub(super) title: String,
    pub(super) detail: Value,
}

#[derive(Debug)]
pub(super) struct AuditLineError(String);

impl fmt::Display for AuditLineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AuditLineError {}

#[derive(Deserialize)]
struct Envelope {
    audit: Option<Value>,
}

pub(super) fn normalize_line(id: u64, line: &[u8]) -> Result<Option<TraceEvent>, AuditLineError> {
    let envelope: Envelope = serde_json::from_slice(line)
        .map_err(|error| AuditLineError(format!("invalid JSON log record: {error}")))?;
    let Some(detail) = envelope.audit else {
        return Ok(None);
    };
    let audit_type = detail
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let audit: AuditRecord = serde_json::from_value(detail.clone())
        .map_err(|error| AuditLineError(format!("invalid {audit_type} audit record: {error}")))?;
    let (root_trace_id, kind, occurred_at, title) = match audit {
        AuditRecord::Process {
            access_time,
            trace_id,
            executable,
            arguments,
            ..
        } => {
            let title = if arguments.is_empty() {
                executable
            } else {
                arguments
                    .iter()
                    .map(|argument| shell_words::quote(argument))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            (trace_root(&trace_id)?, TraceKind::Exec, access_time, title)
        }
        AuditRecord::Filesystem {
            access_time,
            trace_id,
            operation,
            path,
            ..
        } => {
            let kind = match operation {
                crate::audit_log::FileOperation::Open => TraceKind::FileOpen,
                crate::audit_log::FileOperation::Close => TraceKind::FileClose,
            };
            (trace_root(&trace_id)?, kind, access_time, path)
        }
        AuditRecord::Network {
            access_time,
            trace_id,
            destination_ip,
            destination_port,
            domain,
            target_host,
            target_port,
            ..
        } => {
            let title = match (target_host.filter(|host| !host.is_empty()), target_port) {
                (Some(host), Some(port)) => format!("{host}:{port}"),
                _ => match domain.filter(|domain| !domain.is_empty()) {
                    Some(domain) => format!("{domain}:{destination_port}"),
                    None => match destination_ip {
                        IpAddr::V4(address) => format!("{address}:{destination_port}"),
                        IpAddr::V6(address) => format!("[{address}]:{destination_port}"),
                    },
                },
            };
            (
                trace_root(&trace_id)?,
                TraceKind::Network,
                access_time,
                title,
            )
        }
    };

    Ok(Some(TraceEvent {
        id,
        root_trace_id,
        kind,
        occurred_at,
        title,
        detail,
    }))
}

fn trace_root(trace_id: &str) -> Result<String, AuditLineError> {
    let root = trace_id.split(',').next().unwrap_or_default().trim();
    if root.is_empty() {
        return Err(AuditLineError("audit trace_id is empty".to_string()));
    }
    Ok(root.to_string())
}

#[derive(Debug)]
pub(super) enum CursorItem {
    Event(TraceEvent),
    Diagnostic(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

pub(super) struct LogCursor {
    path: PathBuf,
    identity: Option<FileIdentity>,
    offset: u64,
    marker: Vec<u8>,
    pending: Vec<u8>,
    dropping_oversized: bool,
    next_id: u64,
}

impl LogCursor {
    #[cfg(test)]
    fn from_start(path: PathBuf) -> Self {
        Self {
            path,
            identity: None,
            offset: 0,
            marker: Vec::new(),
            pending: Vec::new(),
            dropping_oversized: false,
            next_id: 1,
        }
    }

    pub(super) fn at_end(path: PathBuf) -> io::Result<Self> {
        let mut cursor = Self {
            path,
            identity: None,
            offset: 0,
            marker: Vec::new(),
            pending: Vec::new(),
            dropping_oversized: false,
            next_id: 1,
        };
        match File::open(&cursor.path) {
            Ok(mut file) => {
                let metadata = file.metadata()?;
                cursor.identity = Some(FileIdentity::from_metadata(&metadata));
                cursor.offset = metadata.len();
                cursor.marker = read_marker(&mut file, cursor.offset)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(cursor)
    }

    pub(super) fn poll(&mut self) -> io::Result<Vec<CursorItem>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        let identity = FileIdentity::from_metadata(&metadata);
        let replaced = self.identity.is_some_and(|previous| previous != identity);
        let truncated = metadata.len() < self.offset;
        let overwritten = !replaced
            && !truncated
            && !self.marker.is_empty()
            && read_marker(&mut file, self.offset)? != self.marker;
        if replaced || truncated || overwritten {
            self.reset(identity);
        } else if self.identity.is_none() {
            self.identity = Some(identity);
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended)?;
        self.offset += appended.len() as u64;
        self.marker = read_marker(&mut file, self.offset)?;
        self.consume(appended)
    }

    fn reset(&mut self, identity: FileIdentity) {
        self.identity = Some(identity);
        self.offset = 0;
        self.marker.clear();
        self.pending.clear();
        self.dropping_oversized = false;
    }

    fn consume(&mut self, appended: Vec<u8>) -> io::Result<Vec<CursorItem>> {
        self.pending.extend(appended);
        let mut items = Vec::new();
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if self.dropping_oversized {
                self.dropping_oversized = false;
                continue;
            }
            if line.len() > MAX_LINE_BYTES {
                items.push(CursorItem::Diagnostic(format!(
                    "ignored audit log line larger than {MAX_LINE_BYTES} bytes"
                )));
                continue;
            }
            if line.is_empty() {
                continue;
            }
            match normalize_line(self.next_id, &line) {
                Ok(Some(event)) => {
                    self.next_id += 1;
                    items.push(CursorItem::Event(event));
                }
                Ok(None) => {}
                Err(error) => items.push(CursorItem::Diagnostic(error.to_string())),
            }
        }
        if self.pending.len() > MAX_LINE_BYTES {
            self.pending.clear();
            self.dropping_oversized = true;
            items.push(CursorItem::Diagnostic(format!(
                "ignored audit log line larger than {MAX_LINE_BYTES} bytes"
            )));
        }
        Ok(items)
    }
}

fn read_marker(file: &mut File, offset: u64) -> io::Result<Vec<u8>> {
    let marker_start = offset.saturating_sub(MARKER_BYTES as u64);
    file.seek(SeekFrom::Start(marker_start))?;
    let mut marker = vec![0; (offset - marker_start) as usize];
    file.read_exact(&mut marker)?;
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::{CursorItem, LogCursor, TraceKind, normalize_line};
    use serde_json::json;
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    #[test]
    fn normalizes_process_records_with_trace_root_and_safe_argument_display() {
        let line = serde_json::to_vec(&json!({
            "message": "sandbox audit event",
            "audit": {
                "type": "process",
                "access_time": "2026-08-13T13:56:03.180Z",
                "trace_id": "root-trace, child-trace",
                "pid": 45941,
                "ppid": 45910,
                "process_executable": "/bin/bash",
                "executable": "/bin/echo",
                "arguments": ["/bin/echo", "hello world"],
                "current_dir": "/workspace",
                "operation": "execve"
            }
        }))
        .unwrap();

        let event = normalize_line(7, &line).unwrap().unwrap();

        assert_eq!(event.id, 7);
        assert_eq!(event.root_trace_id, "root-trace");
        assert_eq!(event.kind, TraceKind::Exec);
        assert_eq!(event.occurred_at, "2026-08-13T13:56:03.180Z");
        assert_eq!(event.title, "/bin/echo 'hello world'");
        assert_eq!(event.detail["pid"], 45941);
        assert_eq!(event.detail["current_dir"], "/workspace");
    }

    #[test]
    fn normalizes_file_and_network_records_without_inventing_fields() {
        let file = br#"{"audit":{"type":"filesystem","access_time":"t1","trace_id":"root","pid":2,"operation":"open","path":"/workspace/report.txt","mode":{"access":"read","create":false,"truncate":false,"append":false,"exclusive":false}}}"#;
        let network = br#"{"audit":{"type":"network","access_time":"t2","trace_id":"root, child","pid":3,"destination_ip":"203.0.113.9","destination_port":443,"domain":"api.example.com"}}"#;
        let ip_only = br#"{"audit":{"type":"network","access_time":"t3","trace_id":"root","pid":3,"destination_ip":"2001:db8::1","destination_port":8443,"domain":null}}"#;

        let file = normalize_line(1, file).unwrap().unwrap();
        let network = normalize_line(2, network).unwrap().unwrap();
        let ip_only = normalize_line(3, ip_only).unwrap().unwrap();

        assert_eq!(file.kind, TraceKind::FileOpen);
        assert_eq!(file.title, "/workspace/report.txt");
        assert_eq!(file.detail["mode"]["access"], "read");
        assert_eq!(network.kind, TraceKind::Network);
        assert_eq!(network.title, "api.example.com:443");
        assert_eq!(network.detail.get("url"), None);
        assert_eq!(ip_only.title, "[2001:db8::1]:8443");
    }

    #[test]
    fn http_connect_target_titles_do_not_reuse_the_proxy_port() {
        let line = br#"{"audit":{"type":"network","access_time":"t","trace_id":"root","pid":3,"destination_ip":"127.0.0.1","destination_port":1087,"domain":"chatgpt.com","target_host":"chatgpt.com","target_port":443}}"#;

        let event = normalize_line(1, line).unwrap().unwrap();

        assert_eq!(event.title, "chatgpt.com:443");
        assert_eq!(event.detail["destination_ip"], "127.0.0.1");
        assert_eq!(event.detail["destination_port"], 1087);
        assert_eq!(event.detail["target_host"], "chatgpt.com");
        assert_eq!(event.detail["target_port"], 443);
    }

    #[test]
    fn ignores_non_audit_log_records_and_rejects_malformed_audit_records() {
        assert!(
            normalize_line(1, br#"{"message":"sandbox started"}"#)
                .unwrap()
                .is_none()
        );

        let error = normalize_line(2, br#"{"audit":{"type":"network"}}"#).unwrap_err();
        assert!(error.to_string().contains("invalid network audit record"));
    }

    #[test]
    fn cursor_buffers_partial_lines_and_preserves_append_order() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("sandbox.log");
        fs::write(
            &log,
            br#"{"audit":{"type":"filesystem","access_time":"t1","trace_id":"r","pid":1,"operation":"open","path":"/a","mode":{"access":"read","create":false,"truncate":false,"append":false,"exclusive":false}}}"#,
        )
        .unwrap();
        let mut cursor = LogCursor::from_start(log.clone());

        assert!(cursor.poll().unwrap().is_empty());
        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(br#"{"audit":{"type":"filesystem","access_time":"t2","trace_id":"r","pid":1,"operation":"close","path":"/a","mode":{"access":"read","create":false,"truncate":false,"append":false,"exclusive":false}}}"#).unwrap();
        file.write_all(b"\n").unwrap();

        let items = cursor.poll().unwrap();
        let kinds = items
            .iter()
            .filter_map(|item| match item {
                CursorItem::Event(event) => Some(event.kind),
                CursorItem::Diagnostic(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds, [TraceKind::FileOpen, TraceKind::FileClose]);
    }

    #[test]
    fn cursor_reopens_replaced_or_truncated_logs_without_replaying_old_bytes() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("sandbox.log");
        fs::write(&log, b"old line\n").unwrap();
        let mut cursor = LogCursor::at_end(log.clone()).unwrap();

        fs::write(
            &log,
            b"{\"audit\":{\"type\":\"network\",\"access_time\":\"new\",\"trace_id\":\"r\",\"pid\":1,\"destination_ip\":\"127.0.0.1\",\"destination_port\":80,\"domain\":null}}\n",
        )
        .unwrap();
        let items = cursor.poll().unwrap();

        assert!(
            items.iter().any(
                |item| matches!(item, CursorItem::Event(event) if event.title == "127.0.0.1:80")
            )
        );
    }

    #[test]
    fn cursor_turns_malformed_and_oversized_lines_into_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("sandbox.log");
        let mut bytes = b"not-json\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 256 * 1024 + 1));
        bytes.push(b'\n');
        fs::write(&log, bytes).unwrap();
        let mut cursor = LogCursor::from_start(log);

        let items = cursor.poll().unwrap();

        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, CursorItem::Diagnostic(_)))
                .count(),
            2
        );
    }
}
