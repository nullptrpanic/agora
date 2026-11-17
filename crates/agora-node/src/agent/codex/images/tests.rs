use super::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";

async fn collector(workspace: &Path) -> CodexImages {
    let env = HashMap::from([(
        "CODEX_HOME".into(),
        workspace.join("codex-home").to_string_lossy().into_owned(),
    )]);
    CodexImages::prepare(workspace.to_path_buf(), None, &env).await
}

fn image_count(events: &[OutputEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, OutputEvent::Image { .. }))
        .count()
}

#[tokio::test]
async fn markdown_supports_local_url_forms_and_references_without_changing_other_markup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("图 (1).png");
    std::fs::write(&path, PNG).unwrap();
    let url = reqwest::Url::from_file_path(&path).unwrap();
    for destination in [
        "图%20%281%29.png".to_string(),
        format!("<{}>", path.display()),
        url.to_string(),
        format!("sandbox:{url}"),
        format!("data:image/png;base64,{}", STANDARD.encode(PNG)),
    ] {
        let text =
            format!("before **bold**\n![pic]({destination})\n[site](https://example.com)\nafter");
        let (answer, events) = collector(temp.path()).await.markdown(text).await;
        assert_eq!(image_count(&events), 1, "{destination}: {events:?}");
        assert_eq!(
            answer,
            format!(
                "before **bold**\n{}\n[site](https://example.com)\nafter",
                i18n::IMAGE_REFERENCE
            )
        );
    }
    let text = "![pic][asset]\n\n[asset]: <图 (1).png> \"title\"";
    let (answer, events) = collector(temp.path()).await.markdown(text.into()).await;
    assert_eq!(image_count(&events), 1);
    assert!(answer.starts_with(i18n::IMAGE_REFERENCE));
    let remote = "![remote](http://127.0.0.1:1/private) `![example](missing.png)`";
    let (answer, events) = collector(temp.path()).await.markdown(remote.into()).await;
    assert_eq!(answer, remote);
    assert!(events.is_empty());
}

#[tokio::test]
async fn images_are_deduplicated_across_mcp_and_markdown_sources() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("image.png"), PNG).unwrap();
    let mut images = collector(temp.path()).await;
    let item = serde_json::json!({"status":"completed", "result":{"content":[{"type":"image", "data":STANDARD.encode(PNG)}, {"type":"image", "data":STANDARD.encode(PNG)}]}});
    assert_eq!(image_count(&images.mcp_images(&item)), 1);
    assert!(images.mcp_images(&item).is_empty());
    let (text, events) = images
        .markdown("![one](image.png) ![two](image.png)".into())
        .await;
    assert!(events.is_empty());
    assert_eq!(text.matches(i18n::IMAGE_REFERENCE).count(), 2);
    for status in ["failed", "in_progress"] {
        let mut item = item.clone();
        item["status"] = status.into();
        assert!(images.mcp_images(&item).is_empty());
    }
}

#[tokio::test]
async fn unreadable_unsafe_and_non_image_files_become_warnings_not_answers_with_broken_paths() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let other = outside.path().join("private.png");
    std::fs::write(&other, PNG).unwrap();
    std::fs::write(temp.path().join("text.png"), "not an image").unwrap();
    let mut destinations = vec![
        other.to_string_lossy().into_owned(),
        "missing.png".into(),
        "text.png".into(),
        ".".into(),
        "data:image/png;base64,invalid".into(),
        "data:text/html;base64,WA==".into(),
        "file://remote-host/private.png".into(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&other, temp.path().join("escape.png")).unwrap();
        destinations.push("escape.png".into());
        let fifo = temp.path().join("pipe.png");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        destinations.push("pipe.png".into());
    }
    for destination in destinations {
        let (answer, events) = tokio::time::timeout(
            Duration::from_secs(2),
            collector(temp.path())
                .await
                .markdown(format!("kept ![pic]({destination})")),
        )
        .await
        .unwrap();
        assert_eq!(
            answer,
            format!("kept {}", i18n::IMAGE_REFERENCE_FAILED),
            "{destination}"
        );
        assert_eq!(image_count(&events), 0);
        assert!(
            matches!(
                events.as_slice(),
                [OutputEvent::Progress {
                    status: ProgressStatus::Failed,
                    ..
                }]
            ),
            "{destination}: {events:?}"
        );
    }
}

#[tokio::test]
async fn count_size_and_attempt_budgets_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let mut images = collector(temp.path()).await;
    let mut events = Vec::new();
    for index in 0..=MAX_OUTPUT_IMAGES {
        let mut bytes = PNG.to_vec();
        bytes.push(index as u8);
        images.accept(OutputImage::new(bytes), &mut events);
    }
    assert_eq!(image_count(&events), MAX_OUTPUT_IMAGES);
    assert!(matches!(
        events.last(),
        Some(OutputEvent::Progress {
            status: ProgressStatus::Failed,
            ..
        })
    ));
    let mut images = collector(temp.path()).await;
    let mut events = Vec::new();
    for index in 0..4 {
        let mut bytes = vec![index; MAX_OUTPUT_IMAGE_BYTES];
        bytes[..PNG.len()].copy_from_slice(PNG);
        images.accept(OutputImage::new(bytes), &mut events);
    }
    assert_eq!(image_count(&events), 3);
    assert!(images.total_bytes <= MAX_OUTPUT_IMAGE_TOTAL_BYTES);
    let mut events = Vec::new();
    for _ in 0..100 {
        images.admit_attempt(&mut events);
    }
    assert_eq!(events.len(), 1);
    assert!(CodexImages::decode(&"A".repeat(MAX_OUTPUT_IMAGE_BYTES.div_ceil(3) * 4 + 1)).is_err());
    let oversized = temp.path().join("huge.png");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(MAX_OUTPUT_IMAGE_BYTES as u64 + 1)
        .unwrap();
    assert!(images.read_image(&oversized).await.is_err());
}

#[tokio::test]
async fn fresh_session_collects_only_current_session_new_regular_images() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("codex-home/generated_images");
    let current = root.join("fresh");
    std::fs::create_dir_all(&current).unwrap();
    let old = current.join("old.png");
    std::fs::write(&old, PNG).unwrap();
    std::fs::File::open(old)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(60)),
        )
        .unwrap();
    let mut images = collector(temp.path()).await;
    images.session_started("fresh");
    std::fs::write(current.join("new.png"), PNG).unwrap();
    std::fs::write(current.join("ignored.txt"), PNG).unwrap();
    std::fs::create_dir_all(root.join("other")).unwrap();
    std::fs::write(root.join("other/secret.png"), PNG).unwrap();
    assert_eq!(image_count(&images.native_images().await), 1);
    assert!(images.native_images().await.is_empty());
    images.session_started("../other");
    assert!(images.native_directory().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn generated_root_and_session_symlinks_are_not_followed() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("thread")).unwrap();
    std::fs::write(outside.path().join("thread/image.png"), PNG).unwrap();
    let env = HashMap::from([(
        "CODEX_HOME".into(),
        home.path().to_string_lossy().into_owned(),
    )]);
    let root = home.path().join("generated_images");
    std::os::unix::fs::symlink(outside.path(), &root).unwrap();
    let mut images = CodexImages::prepare(workspace.path().into(), Some("thread"), &env).await;
    assert!(
        images
            .read_image(&root.join("thread/image.png"))
            .await
            .is_err()
    );
    assert!(matches!(
        images.native_images().await.as_slice(),
        [OutputEvent::Progress {
            status: ProgressStatus::Failed,
            ..
        }]
    ));
    std::fs::remove_file(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(outside.path().join("thread"), root.join("thread")).unwrap();
    let mut images = CodexImages::prepare(workspace.path().into(), Some("thread"), &env).await;
    assert!(
        images
            .read_image(&root.join("thread/image.png"))
            .await
            .is_err()
    );
    assert_eq!(image_count(&images.native_images().await), 0);
}

#[test]
fn image_bytes_are_checked_and_never_dumped_by_debug() {
    for (bytes, mime, name) in [
        (PNG.to_vec(), "image/png", "image.png"),
        (b"\xff\xd8\xffsecret".to_vec(), "image/jpeg", "image.jpg"),
        (b"RIFF0000WEBPsecret".to_vec(), "image/webp", "image.webp"),
    ] {
        let image = OutputImage::new(bytes).unwrap();
        assert_eq!(image.media_type(), mime);
        assert_eq!(image.file_name(), name);
        assert!(!format!("{image:?}").contains("secret"));
    }
    assert!(OutputImage::new(vec![]).is_err());
    assert!(OutputImage::new(vec![0; MAX_OUTPUT_IMAGE_BYTES + 1]).is_err());
}
