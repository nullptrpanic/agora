use std::io;
use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::ser::Error;
use serde::{Serialize, Serializer};

#[derive(Clone)]
struct SharedWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn content(&self) -> String {
        let buffer = self.buffer.lock().unwrap();
        String::from_utf8_lossy(&buffer).to_string()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct BrokenValue;

impl Serialize for BrokenValue {
    fn serialize<S>(&self, _: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Err(S::Error::custom("broken value"))
    }
}

#[test]
fn logger_macros_are_available_from_logger_module() {
    let writer = SharedWriter::new();
    agora_core::logger::init(writer.clone(), agora_core::logger::LevelFilter::Debug).unwrap();

    agora_core::logger::info!("node started");
    agora_core::logger::debug!("run {}", 1);

    let second_writer = SharedWriter::new();
    assert!(
        agora_core::logger::init(
            second_writer.clone(),
            agora_core::logger::LevelFilter::Error
        )
        .is_ok()
    );

    agora_core::logger::debug!("still debug");

    let arguments = format_args!("external record");
    let record = agora_core::logger::log::Record::builder()
        .args(arguments)
        .level(agora_core::logger::log::Level::Warn)
        .file(Some("external.rs"))
        .line(Some(7))
        .build();
    agora_core::logger::log::logger().log(&record);

    let arguments = format_args!("workspace record");
    let record = agora_core::logger::log::Record::builder()
        .args(arguments)
        .level(agora_core::logger::log::Level::Info)
        .file(Some("crates/agora-core/src/lib.rs"))
        .build();
    agora_core::logger::log::logger().log(&record);

    let entry = agora_core::logger::LoggerEntry::new().with_entry("bad", BrokenValue);
    agora_core::logger::info!(entry = entry, "bad entry");

    let entry = agora_core::logger::LoggerEntry::default()
        .with_entry("first", 1_u32)
        .with_entry("second", "two");
    assert!(!entry.is_error());
    assert_eq!(
        serde_json::to_value(entry.clone()).unwrap(),
        serde_json::json!({"first": 1, "second": "two"})
    );
    agora_core::logger::output!(entry = entry, "structured output");

    let error_entry = agora_core::logger::LoggerEntry::new().with_error("broken");
    assert!(error_entry.is_error());
    agora_core::logger::output!(entry = error_entry, "failed output");
    agora_core::logger::log::logger().flush();

    let content = writer.content();
    assert!(content.contains("\"level\":\"INFO\""));
    assert!(content.contains("\"message\":\"node started\""));
    assert!(content.contains("\"level\":\"DEBUG\""));
    assert!(content.contains("\"message\":\"run 1\""));
    assert!(content.contains("\"message\":\"still debug\""));
    assert!(content.contains("\"message\":\"external record\""));
    assert!(content.contains("\"file\":\"external.rs\""));
    assert!(content.contains("\"file\":\"agora-core/src/lib.rs\""));
    assert!(content.contains("\"line\":7"));
    assert!(content.contains("\"message\":\"bad entry\""));
    assert!(content.contains("\"logger_error\""));
    assert!(content.contains("\"first\":1"));
    assert!(content.contains("\"second\":\"two\""));
    assert!(content.contains("\"message\":\"structured output\""));
    assert!(content.contains("\"message\":\"failed output\""));
    assert!(content.contains("\"error\":\"broken\""));
    assert!(!content.contains("\n\n"));
    assert_eq!(second_writer.content(), "");
}
