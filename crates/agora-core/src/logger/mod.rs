#![deny(unused)]

mod entry;
use chrono::Local;
pub use entry::*;
pub use log;
pub use log::LevelFilter;
use log::kv::{Key, Value};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::io;
use std::io::Write;
use std::sync::{Arc, Mutex};

pub use crate::{debug, error, info, output};
const ENTRY_KEY: &str = "entry";

#[macro_export]
macro_rules! info {
    (entry = $entry:expr, $($arg:tt)+) => ($crate::logger::log::log!($crate::logger::log::Level::Info, entry=$entry; $($arg)+));
    ($($arg:tt)+) => ($crate::logger::log::log!($crate::logger::log::Level::Info, $($arg)+))
}

#[macro_export]
macro_rules! debug {
    (entry = $entry:expr, $($arg:tt)+) => ($crate::logger::log::log!($crate::logger::log::Level::Debug, entry=$entry; $($arg)+));
    ($($arg:tt)+) => ($crate::logger::log::log!($crate::logger::log::Level::Debug, $($arg)+))
}

#[macro_export]
macro_rules! error {
    (entry = $entry:expr, $($arg:tt)+) => ($crate::logger::log::log!($crate::logger::log::Level::Error, entry=$entry; $($arg)+));
    ($($arg:tt)+) => ($crate::logger::log::log!($crate::logger::log::Level::Error, $($arg)+))
}

#[macro_export]
macro_rules! output {
    (entry = $entry:expr, $($arg:tt)+) => {
        {
            let entry = $entry;
            if entry.is_error() {
                $crate::logger::log::log!($crate::logger::log::Level::Error, entry=entry; $($arg)+)
            } else {
                $crate::logger::log::log!($crate::logger::log::Level::Info, entry=entry; $($arg)+)
            }
        }
    };
}

struct JLogger<W> {
    writer: Arc<Mutex<W>>,
}

struct RecordEx<'a> {
    record: &'a log::Record<'a>,
}

impl Serialize for RecordEx<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("message", self.record.args())?;
        map.serialize_entry(
            "time",
            &Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        )?;
        if let Some(file) = self.record.file() {
            if let Some(file) = file.strip_prefix("crates/") {
                map.serialize_entry("file", file)?
            } else {
                map.serialize_entry("file", file)?;
            }
        }
        if let Some(line) = self.record.line() {
            map.serialize_entry("line", &line)?;
        }
        map.serialize_entry("level", &self.record.level())?;
        map.end()
    }
}

#[derive(Serialize)]
struct RecordEntry<'a> {
    #[serde(flatten)]
    entry: &'a Value<'a>,
    #[serde(flatten)]
    record: RecordEx<'a>,
}

#[derive(Serialize)]
struct RecordFallback<'a> {
    #[serde(flatten)]
    record: RecordEx<'a>,
    logger_error: String,
}

impl<W: Send + Write> log::Log for JLogger<W> {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        if let Ok(mut guard) = self.writer.lock() {
            let source = record.key_values();
            let entry = source.get(Key::from_str(ENTRY_KEY));
            let result = if let Some(entry) = entry {
                serde_json::to_string(&RecordEntry {
                    record: RecordEx { record },
                    entry: &entry,
                })
            } else {
                serde_json::to_string(&RecordEx { record })
            };
            let line = result.unwrap_or_else(|err| {
                serde_json::to_string(&RecordFallback {
                    record: RecordEx { record },
                    logger_error: err.to_string(),
                })
                .unwrap_or_else(|_| {
                    "{\"message\":\"logger serialization failed\",\"level\":\"ERROR\"}".to_string()
                })
            });
            let _ = writeln!(guard, "{}", line);
        }
    }

    fn flush(&self) {}
}

pub fn init(writer: impl Write + Send + 'static, level: LevelFilter) -> io::Result<()> {
    match try_init(writer, level) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

fn try_init(writer: impl Write + Send + 'static, level: LevelFilter) -> io::Result<()> {
    log::set_boxed_logger(Box::new(JLogger {
        writer: Arc::new(Mutex::new(writer)),
    }))
    .map_err(|err| io::Error::new(io::ErrorKind::AlreadyExists, format!("{}", err)))?;
    log::set_max_level(level);
    Ok(())
}
