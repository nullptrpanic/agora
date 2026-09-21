use super::TaskAttachment;

pub(crate) const MAX_OUTPUT_IMAGE_BYTES: usize = 10_000_000;
pub(crate) const MAX_OUTPUT_IMAGES: usize = 8;
pub(crate) const MAX_OUTPUT_IMAGE_TOTAL_BYTES: usize = 32_000_000;

/// Validated, owned image bytes. Debug output never contains image data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputImage(TaskAttachment);

impl OutputImage {
    pub fn new(data: Vec<u8>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            data.len() <= MAX_OUTPUT_IMAGE_BYTES,
            "output image exceeds 10 MB"
        );
        let (name, media_type) = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            ("image.png", "image/png")
        } else if data.starts_with(b"\xff\xd8\xff") {
            ("image.jpg", "image/jpeg")
        } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
            ("image.webp", "image/webp")
        } else {
            anyhow::bail!("output image must be PNG, JPEG, or WebP");
        };
        Ok(Self(TaskAttachment::image(name, media_type, data)))
    }

    pub fn data(&self) -> &[u8] {
        self.0.data()
    }
    pub fn media_type(&self) -> &str {
        self.0.media_type()
    }
    pub fn file_name(&self) -> &str {
        self.0.file_name()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    Thinking {
        text: String,
    },
    Progress {
        id: String,
        text: String,
        status: ProgressStatus,
    },
    CommandExecution {
        id: String,
        command: String,
        status: ProgressStatus,
        exit_code: Option<i32>,
    },
    Answer {
        text: String,
    },
    Image {
        id: String,
        image: OutputImage,
    },
    Usage(TokenUsage),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}
