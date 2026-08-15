use log::kv::{ToValue, Value};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

trait SerializeClone: erased_serde::Serialize + Send {
    fn clone_box(&self) -> Box<dyn SerializeClone>;
}

impl Serialize for dyn SerializeClone {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        erased_serde::serialize(self, serializer)
    }
}

impl<T: erased_serde::Serialize + Clone + Send + 'static> SerializeClone for T {
    fn clone_box(&self) -> Box<dyn SerializeClone> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn SerializeClone> {
    fn clone(&self) -> Self {
        self.as_ref().clone_box()
    }
}

#[derive(Clone)]
pub struct LoggerEntry {
    key: String,
    value: Box<dyn SerializeClone>,
    next: Option<Box<LoggerEntry>>,
    is_error: bool,
}

impl LoggerEntry {
    pub fn new() -> LoggerEntry {
        Self {
            key: "".to_string(),
            value: Box::new(""),
            next: None,
            is_error: false,
        }
    }
}

impl Default for LoggerEntry {
    fn default() -> Self {
        Self::new()
    }
}

impl LoggerEntry {
    pub fn with_entry<T>(self, key: &str, value: T) -> LoggerEntry
    where
        T: erased_serde::Serialize + Clone + Send + 'static,
    {
        Self {
            key: key.to_string(),
            value: Box::new(value),
            is_error: self.is_error,
            next: Some(Box::new(self)),
        }
    }

    pub fn with_error(self, err: impl ToString) -> LoggerEntry {
        Self {
            key: "error".to_string(),
            value: Box::new(err.to_string()),
            next: Some(Box::new(self)),
            is_error: true,
        }
    }

    pub fn is_error(&self) -> bool {
        self.is_error
    }
}

impl Serialize for LoggerEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        let mut next = Some(self);
        while let Some(entry) = next {
            if !entry.key.is_empty() {
                map.serialize_entry(&entry.key, entry.value.as_ref())?;
            }
            next = entry.next.as_deref();
        }
        map.end()
    }
}

impl ToValue for LoggerEntry {
    fn to_value(&self) -> Value<'_> {
        Value::from_serde(self)
    }
}
