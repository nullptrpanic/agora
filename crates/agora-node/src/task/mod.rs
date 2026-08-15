mod output;

pub use output::{OutputEvent, ProgressStatus, TokenUsage};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

const RECEIPT_LOG_TEXT_MAX_BYTES: usize = 2 * 1024;
const RECEIPT_LOG_TRUNCATION_MARKER: &str = "[truncated]";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CommandRequest {
    path: Vec<String>,
    arguments: BTreeMap<String, String>,
}

impl CommandRequest {
    pub fn new<I, S>(path: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            arguments: BTreeMap::new(),
        }
    }

    pub fn with_argument(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.arguments.insert(name.into(), value.into());
        self
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn arguments(&self) -> &BTreeMap<String, String> {
        &self.arguments
    }

    pub fn argument(&self, name: &str) -> Option<&str> {
        self.arguments.get(name).map(String::as_str)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelTaskInput {
    Message(TaskContent),
    Command(CommandRequest),
}

impl ChannelTaskInput {
    pub fn message(&self) -> Option<&TaskContent> {
        match self {
            Self::Message(content) => Some(content),
            Self::Command(_) => None,
        }
    }

    pub fn command(&self) -> Option<&CommandRequest> {
        match self {
            Self::Message(_) => None,
            Self::Command(command) => Some(command),
        }
    }

    pub(crate) fn receipt_log_fields(&self) -> (String, usize, usize) {
        match self {
            Self::Message(content) => (
                receipt_log_text(content.text()),
                content.text().len(),
                content.attachments().len(),
            ),
            Self::Command(_) => (String::new(), 0, 0),
        }
    }
}

fn receipt_log_text(text: &str) -> String {
    let mut logged = text.escape_debug().collect::<String>();
    if logged.len() <= RECEIPT_LOG_TEXT_MAX_BYTES {
        return logged;
    }
    let mut end = RECEIPT_LOG_TEXT_MAX_BYTES.saturating_sub(RECEIPT_LOG_TRUNCATION_MARKER.len());
    while !logged.is_char_boundary(end) {
        end -= 1;
    }
    logged.truncate(end);
    logged.push_str(RECEIPT_LOG_TRUNCATION_MARKER);
    logged
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskAttachmentKind {
    Image,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TaskAttachment {
    kind: TaskAttachmentKind,
    file_name: String,
    media_type: String,
    data: Arc<[u8]>,
}

impl TaskAttachment {
    pub fn image(
        file_name: impl Into<String>,
        media_type: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            kind: TaskAttachmentKind::Image,
            file_name: file_name.into(),
            media_type: media_type.into(),
            data: Arc::from(data.into()),
        }
    }

    pub fn kind(&self) -> TaskAttachmentKind {
        self.kind
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl fmt::Debug for TaskAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskAttachment")
            .field("kind", &self.kind)
            .field("file_name", &self.file_name)
            .field("media_type", &self.media_type)
            .field("data_len", &self.data.len())
            .finish()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskContent {
    text: String,
    attachments: Vec<TaskAttachment>,
}

impl TaskContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    pub fn with_attachment(mut self, attachment: TaskAttachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn attachments(&self) -> &[TaskAttachment] {
        &self.attachments
    }

    pub(crate) fn into_parts(self) -> (String, Vec<TaskAttachment>) {
        (self.text, self.attachments)
    }
}

impl From<String> for TaskContent {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for TaskContent {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

#[cfg(test)]
mod tests;
