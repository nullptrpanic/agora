use super::*;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";

#[cfg(unix)]
async fn execute(workspace: &std::path::Path, items: Vec<Value>) -> VecAgentOutput {
    use std::os::unix::fs::PermissionsExt;
    let script = workspace.join("codex");
    let mut events = vec![json!({"type":"thread.started", "thread_id":"image-thread"})];
    events.extend(
        items
            .into_iter()
            .map(|item| json!({"type":"item.completed", "item":item})),
    );
    events.push(json!({"type":"turn.completed"}));
    let lines = events
        .iter()
        .map(|event| {
            format!(
                "printf '%s\\n' '{}'\n",
                event.to_string().replace('\'', "'\\''")
            )
        })
        .collect::<String>();
    std::fs::write(&script, format!("#!/bin/sh\ncat >/dev/null\n{lines}")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let configured =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, workspace)).unwrap();
    let mut output = VecAgentOutput::default();
    let outcome = completed(
        configured
            .run(
                AgentTask::new("generate an image"),
                None,
                AgentRunControl::new(),
                &mut output,
            )
            .await
            .unwrap(),
    );
    assert_eq!(outcome.exit_code(), 0);
    output
}

fn answer(text: &str) -> Value {
    json!({"id":"answer", "type":"agent_message", "text":text})
}

fn tool(data: &str) -> Value {
    json!({
        "id":"image-tool", "type":"mcp_tool_call", "server":"images", "tool":"generate",
        "status":"completed", "arguments":{}, "error":null,
        "result":{"content":[{"type":"image", "mimeType":"image/png", "data":data}], "structured_content":null}
    })
}

#[cfg(unix)]
#[tokio::test]
async fn codex_preserves_completed_mcp_image_content() {
    let temp = tempfile::tempdir().unwrap();
    let output = execute(temp.path(), vec![tool(PNG), answer("图片已生成。")]).await;
    assert_eq!(output.answer_text(), "图片已生成。");
    assert_eq!(
        output.events.len(),
        3,
        "image content was lost: {:?}",
        output.events
    );
    let OutputEvent::Image { image, .. } = &output.events[1] else {
        panic!("missing image event")
    };
    assert_eq!(image.data(), STANDARD.decode(PNG).unwrap());
    assert_eq!(image.media_type(), "image/png");
}

#[cfg(unix)]
#[tokio::test]
async fn codex_materializes_markdown_images_but_preserves_code_and_links() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("my picture.png"),
        STANDARD.decode(PNG).unwrap(),
    )
    .unwrap();
    let text = "正文\n![图](<my picture.png>)\n\n`![示例](my.png)`\n\n```md\n![示例](my.png)\n```\n\n[链接](https://example.com)";
    let output = execute(temp.path(), vec![answer(text)]).await;
    assert!(
        !output.answer_text().contains("![图]"),
        "local image reference was not materialized"
    );
    assert!(output.answer_text().contains("`![示例](my.png)`"));
    assert!(output.answer_text().contains("```md\n![示例](my.png)\n```"));
    assert!(output.answer_text().contains("[链接](https://example.com)"));
    assert_eq!(output.events.len(), 2);
    assert!(
        matches!(&output.events[0], OutputEvent::Answer { .. }),
        "publish text before awaiting final image uploads"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn codex_collects_native_images_without_replaying_old_or_other_session_images() {
    let temp = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "images::native_image_fixture", "--nocapture"])
            .env("AGORA_NATIVE_IMAGE_FIXTURE", temp.path())
            .env("CODEX_HOME", temp.path().join("codex-home"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.status.success(),
        "native image fixture failed: {} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_image_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let Some(workspace) =
        std::env::var_os("AGORA_NATIVE_IMAGE_FIXTURE").map(std::path::PathBuf::from)
    else {
        return;
    };
    let generated = workspace.join("codex-home/generated_images");
    for session in ["native-thread", "other-thread"] {
        std::fs::create_dir_all(generated.join(session)).unwrap();
        let mut previous = STANDARD.decode(PNG).unwrap();
        previous.push(if session == "native-thread" { 1 } else { 2 });
        std::fs::write(generated.join(session).join("old.png"), previous).unwrap();
    }
    std::fs::write(workspace.join("new.png"), STANDARD.decode(PNG).unwrap()).unwrap();
    let script = workspace.join("native-codex");
    std::fs::write(&script, concat!(
        "#!/bin/sh\ncat >/dev/null\n",
        "cp new.png \"$CODEX_HOME/generated_images/native-thread/new.png\"\n",
        "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"native-thread\"}'\n",
        "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}'\n",
        "printf '%s\\n' '{\"type\":\"turn.completed\"}'\n",
    )).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let configured =
        ConfiguredAgent::from_config(agent(AgentType::Codex, &script, &workspace)).unwrap();
    for expected in [1, 0] {
        let mut output = VecAgentOutput::default();
        configured
            .run(
                AgentTask::new("image"),
                Some("native-thread".into()),
                AgentRunControl::new(),
                &mut output,
            )
            .await
            .unwrap();
        let images = output
            .events
            .iter()
            .filter_map(|event| {
                if let OutputEvent::Image { image, .. } = event {
                    Some(image)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            images.len(),
            expected,
            "must emit only new filenames from the current thread"
        );
        assert!(
            matches!(&output.events[0], OutputEvent::Answer { .. }),
            "native image delivery must not hold back final text"
        );
        if let Some(image) = images.first() {
            assert_eq!(image.data(), STANDARD.decode(PNG).unwrap());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn codex_reports_invalid_image_content_without_losing_the_answer() {
    let temp = tempfile::tempdir().unwrap();
    let output = execute(
        temp.path(),
        vec![tool("not-base64"), answer("回答仍然保留")],
    )
    .await;
    assert_eq!(output.answer_text(), "回答仍然保留");
    assert!(
        output.events.iter().any(|event| matches!(
            event,
            OutputEvent::Progress {
                status: ProgressStatus::Failed,
                ..
            }
        )),
        "invalid image was silently lost"
    );
}
