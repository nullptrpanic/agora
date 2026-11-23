#![cfg(unix)]

use agora_node::agent::{AgentOutput, AgentRunControl, AgentTask, ConfiguredAgent};
use agora_node::task::OutputEvent;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Output(Vec<OutputEvent>);
impl AgentOutput for Output {
    async fn write(&mut self, event: OutputEvent) -> anyhow::Result<()> {
        self.0.push(event);
        Ok(())
    }
}

// This test binary alone owns its process-global logger.
#[tokio::test]
async fn nonfatal_diagnostics_are_typed_and_logged_without_displacing_the_answer() {
    let capture = Capture::default();
    agora_core::logger::init(capture.clone(), agora_core::logger::LevelFilter::Warn).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'early diagnostic without newline' >&2\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = serde_json::from_value(serde_json::json!({"name":"test", "isolate":"none", "workspace":temp.path(), "type":"codex", "path":script, "subscribe":[]})).unwrap();
    let agent = ConfiguredAgent::from_config(config).unwrap();
    let control = AgentRunControl::new();
    let running = tokio::spawn({
        let control = control.clone();
        async move {
            agent
                .run(
                    AgentTask::new("hello"),
                    None,
                    control,
                    &mut Output::default(),
                )
                .await
        }
    });
    let logged = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if String::from_utf8_lossy(&capture.0.lock().unwrap())
                .contains("early diagnostic without newline")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    control.stop();
    running.await.unwrap().unwrap();
    assert!(logged.is_ok(), "stderr was lost until completion");

    use serde_json::json;
    let warning = "Codex is ignoring 2 unrecognized configuration settings. Check for typos or deprecated settings.\nuser (/tmp/example/config.toml): disable_feedback_command is ignored.\nuser (/tmp/example/config.toml): disable_telemetry is ignored.";
    let events = [
        json!({"type":"thread.started", "thread_id":"diagnostics"}),
        json!({"type":"item.completed", "item":{"id":"answer", "type":"agent_message", "text":"天气查询完成。"}}),
        json!({"type":"item.completed", "item":{"id":"warning-1", "type":"error", "message":warning}}),
        json!({"type":"item.completed", "item":{"id":"warning-2", "type":"error", "message":warning}}),
        json!({"type":"turn.completed"}),
    ];
    std::fs::write(
        temp.path().join("events.jsonl"),
        events
            .iter()
            .map(|event| format!("{event}\n"))
            .collect::<String>(),
    )
    .unwrap();
    std::fs::write(&script, "#!/bin/sh\ncat >/dev/null\ncat events.jsonl\n").unwrap();
    let config = serde_json::from_value(json!({"name":"test", "isolate":"none", "workspace":temp.path(), "type":"codex", "path":script, "subscribe":[]})).unwrap();
    let agent = ConfiguredAgent::from_config(config).unwrap();
    let mut output = Output::default();
    agent
        .run(
            AgentTask::new("hello"),
            None,
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap();
    assert_eq!(
        output.0,
        vec![
            OutputEvent::Diagnostic {
                id: "warning-1".into(),
                text: warning.into()
            },
            OutputEvent::Diagnostic {
                id: "warning-2".into(),
                text: warning.into()
            },
            OutputEvent::Answer {
                text: "天气查询完成。".into()
            },
        ]
    );
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.lines()
            .filter(|line| line.contains("Codex is ignoring 2"))
            .count(),
        1
    );
    assert!(logs.contains("disable_telemetry"));

    // A fatal stream event remains visible even if it has the same text as a warning.
    let events = [
        json!({"type":"thread.started", "thread_id":"fatal"}),
        json!({"type":"turn.failed", "error":{"message":warning}}),
    ];
    std::fs::write(
        temp.path().join("events.jsonl"),
        events
            .iter()
            .map(|event| format!("{event}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let mut output = Output::default();
    agent
        .run(
            AgentTask::new("hello"),
            None,
            AgentRunControl::new(),
            &mut output,
        )
        .await
        .unwrap();
    assert_eq!(
        output.0,
        vec![OutputEvent::Answer {
            text: warning.into()
        }]
    );
}
