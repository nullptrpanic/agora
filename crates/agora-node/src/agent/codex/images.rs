use crate::i18n;
use crate::task::{
    MAX_OUTPUT_IMAGE_BYTES, MAX_OUTPUT_IMAGE_TOTAL_BYTES, MAX_OUTPUT_IMAGES, OutputEvent,
    OutputImage, ProgressStatus,
};
use agora_core::logger;
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pulldown_cmark::{Event, Parser, Tag};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;

pub(super) struct CodexImages {
    workspace: PathBuf,
    generated_root: Option<PathBuf>,
    native_session: Option<String>,
    previous_files: Option<HashSet<OsString>>,
    started_at: SystemTime,
    native_error: Option<anyhow::Error>,
    native_collected: bool,
    emitted: Vec<OutputImage>,
    total_bytes: usize,
    attempts: usize,
    warnings: usize,
}

#[cfg(test)]
mod tests;

impl CodexImages {
    pub(super) async fn prepare(
        workspace: PathBuf,
        session: Option<&str>,
        env: &HashMap<String, String>,
    ) -> Self {
        let variable = |name| {
            env.get(name)
                .map(OsString::from)
                .or_else(|| std::env::var_os(name))
        };
        let codex_home = variable("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| variable("HOME").map(|home| PathBuf::from(home).join(".codex")));
        let mut images = Self {
            generated_root: codex_home.map(|home| workspace.join(home).join("generated_images")),
            workspace,
            native_session: None,
            previous_files: None,
            started_at: SystemTime::now(),
            native_error: None,
            native_collected: false,
            emitted: Vec::new(),
            total_bytes: 0,
            attempts: 0,
            warnings: 0,
        };
        if let Some(session) = session {
            images.session_started(session);
            // Snapshot before spawning Codex, not when reading thread.started.
            match images.native_files().await {
                Ok(paths) => {
                    images.previous_files = Some(
                        paths
                            .into_iter()
                            .filter_map(|path| path.file_name().map(OsString::from))
                            .collect(),
                    )
                }
                Err(error) => images.native_error = Some(error),
            }
        }
        images
    }

    pub(super) fn session_started(&mut self, session: &str) {
        if self.native_session.as_deref() == Some(session) {
            return;
        }
        self.previous_files = None;
        self.native_session = (!session.is_empty()
            && session.len() <= 128
            && session
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        .then(|| session.to_string());
    }

    fn native_directory(&self) -> Option<PathBuf> {
        Some(
            self.generated_root
                .as_ref()?
                .join(self.native_session.as_ref()?),
        )
    }

    async fn checked_native_directory(&self) -> Result<Option<PathBuf>> {
        let Some(directory) = self.native_directory() else {
            return Ok(None);
        };
        let root = self.generated_root.as_ref().unwrap();
        // Never follow a generated_images or session directory symlink.
        for path in [root, &directory] {
            match tokio::fs::symlink_metadata(path).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                result => ensure!(
                    result?.is_dir(),
                    "generated-image directory is not a real directory"
                ),
            }
        }
        let root = tokio::fs::canonicalize(root).await?;
        let directory = tokio::fs::canonicalize(directory).await?;
        ensure!(
            directory.starts_with(root),
            "generated-image session directory escaped its root"
        );
        Ok(Some(directory))
    }

    async fn native_files(&self) -> Result<Vec<PathBuf>> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let Some(directory) = self.checked_native_directory().await? else {
                return Ok(Vec::new());
            };
            let mut entries = tokio::fs::read_dir(directory).await?;
            let mut paths = Vec::new();
            while let Some(entry) = entries.next_entry().await? {
                ensure!(
                    paths.len() < 4096,
                    "generated-image directory exceeds scan limit"
                );
                paths.push(entry.path());
            }
            paths.sort();
            Ok(paths)
        })
        .await
        .context("generated-image directory scan timed out")?
    }

    pub(super) async fn native_images(&mut self) -> Vec<OutputEvent> {
        if self.native_collected {
            return Vec::new();
        }
        self.native_collected = true;
        let mut events = Vec::new();
        let paths = if let Some(error) = self.native_error.take() {
            Err(error)
        } else {
            self.native_files().await
        };
        let paths = match paths {
            Ok(paths) => paths,
            Err(error) => {
                self.warn(&error, &mut events);
                return events;
            }
        };
        for path in paths {
            if self
                .previous_files
                .as_ref()
                .is_some_and(|files| path.file_name().is_some_and(|name| files.contains(name)))
            {
                continue;
            }
            let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
                continue;
            };
            if !metadata.is_file() || path.extension().is_none_or(|extension| extension != "png") {
                continue;
            }
            if self.previous_files.is_none()
                && !metadata
                    .modified()
                    .is_ok_and(|modified| modified >= self.started_at)
            {
                continue;
            }
            if !self.admit_attempt(&mut events) {
                break;
            }
            let image = self.read_image(&path).await;
            self.accept(image, &mut events);
        }
        events
    }

    pub(super) fn mcp_images(&mut self, item: &serde_json::Value) -> Vec<OutputEvent> {
        let mut events = Vec::new();
        if item.get("status").and_then(serde_json::Value::as_str) != Some("completed")
            || item.get("error").is_some_and(|error| !error.is_null())
        {
            return events;
        }
        if let Some(content) = item
            .pointer("/result/content")
            .and_then(serde_json::Value::as_array)
        {
            for block in content {
                if block.get("type").and_then(serde_json::Value::as_str) != Some("image") {
                    continue;
                }
                if !self.admit_attempt(&mut events) {
                    break;
                }
                let result = block
                    .get("data")
                    .and_then(serde_json::Value::as_str)
                    .context("MCP image has no data")
                    .and_then(Self::decode);
                self.accept(result, &mut events);
            }
        }
        events
    }

    pub(super) async fn markdown(&mut self, text: String) -> (String, Vec<OutputEvent>) {
        // Offsets let us leave all non-image source bytes (including code) untouched.
        let references = Parser::new(&text)
            .into_offset_iter()
            .filter_map(|(event, range)| match event {
                Event::Start(Tag::Image { dest_url, .. }) => Some((range, dest_url.into_string())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut result = String::with_capacity(text.len());
        let mut events = Vec::new();
        let mut cursor = 0;
        for (range, destination) in references {
            if range.start < cursor {
                continue;
            }
            let destination = destination.strip_prefix("sandbox:").unwrap_or(&destination);
            let source = if let Some(data) = destination.strip_prefix("data:") {
                Some(Err(data))
            } else {
                reqwest::Url::from_directory_path(&self.workspace)
                    .ok()
                    .and_then(|base| base.join(destination).ok())
                    .filter(|url| url.scheme() == "file")
                    .map(|url| Ok(url.to_file_path()))
            };
            let Some(source) = source else {
                continue;
            };
            result.push_str(&text[cursor..range.start]);
            let accepted = if self.admit_attempt(&mut events) {
                let loaded = match source {
                    Err(data) => data
                        .split_once(";base64,")
                        .filter(|(mime, _)| {
                            matches!(*mime, "image/png" | "image/jpeg" | "image/webp")
                        })
                        .context("unsupported image data URL")
                        .and_then(|(_, data)| Self::decode(data)),
                    Ok(Ok(path)) => self.read_image(&path).await,
                    Ok(Err(())) => Err(anyhow::anyhow!("non-local file URL")),
                };
                self.accept(loaded, &mut events)
            } else {
                false
            };
            result.push_str(if accepted {
                i18n::IMAGE_REFERENCE
            } else {
                i18n::IMAGE_REFERENCE_FAILED
            });
            cursor = range.end;
        }
        result.push_str(&text[cursor..]);
        (result, events)
    }

    fn decode(data: &str) -> Result<OutputImage> {
        ensure!(
            data.len() <= MAX_OUTPUT_IMAGE_BYTES.div_ceil(3) * 4,
            "image base64 exceeds limit"
        );
        OutputImage::new(STANDARD.decode(data).context("invalid image base64")?)
    }

    async fn read_image(&self, path: &Path) -> Result<OutputImage> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let path = tokio::fs::canonicalize(path)
                .await
                .context("image path unavailable")?;
            let workspace = tokio::fs::canonicalize(&self.workspace).await?;
            let mut allowed = path.starts_with(&workspace);
            if !allowed && let Some(root) = self.checked_native_directory().await? {
                allowed = path.starts_with(root);
            }
            ensure!(
                allowed,
                "image outside workspace or generated-image directory"
            );
            let mut options = tokio::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            let file = options
                .open(&path)
                .await
                .context("open output image failed")?;
            let metadata = file.metadata().await?;
            ensure!(metadata.is_file(), "output image is not a regular file");
            ensure!(
                metadata.len() <= MAX_OUTPUT_IMAGE_BYTES as u64,
                "output image exceeds limit"
            );
            let mut data = Vec::new();
            file.take((MAX_OUTPUT_IMAGE_BYTES + 1) as u64)
                .read_to_end(&mut data)
                .await?;
            OutputImage::new(data)
        })
        .await
        .context("reading output image timed out")?
    }

    fn admit_attempt(&mut self, events: &mut Vec<OutputEvent>) -> bool {
        self.attempts = self.attempts.saturating_add(1);
        if self.attempts > 32 {
            if self.attempts == 33 {
                self.warn(&anyhow::anyhow!("too many image references"), events);
            }
            return false;
        }
        true
    }

    fn accept(&mut self, result: Result<OutputImage>, events: &mut Vec<OutputEvent>) -> bool {
        let result = result.and_then(|image| {
            if self
                .emitted
                .iter()
                .any(|previous| previous.data() == image.data())
            {
                return Ok(None);
            }
            ensure!(
                self.emitted.len() < MAX_OUTPUT_IMAGES,
                "too many output images"
            );
            ensure!(
                image.data().len() <= MAX_OUTPUT_IMAGE_TOTAL_BYTES - self.total_bytes,
                "output images exceed total limit"
            );
            Ok(Some(image))
        });
        match result {
            Ok(Some(image)) => {
                self.total_bytes += image.data().len();
                self.emitted.push(image.clone());
                events.push(OutputEvent::Image {
                    id: format!("image-{}", self.emitted.len()),
                    image,
                });
                true
            }
            Ok(None) => true,
            Err(error) => {
                self.warn(&error, events);
                false
            }
        }
    }

    fn warn(&mut self, error: &anyhow::Error, events: &mut Vec<OutputEvent>) {
        logger::error!("codex output image unavailable: {}", error);
        self.warnings += 1;
        events.push(OutputEvent::Progress {
            id: format!("image-error-{}", self.warnings),
            text: i18n::IMAGE_UNAVAILABLE.to_string(),
            status: ProgressStatus::Failed,
        });
    }
}
