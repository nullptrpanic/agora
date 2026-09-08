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

struct Output;
impl AgentOutput for Output {
    async fn write(&mut self, _: OutputEvent) -> anyhow::Result<()> {
        Ok(())
    }
}

// This test binary alone owns its process-global logger.
#[tokio::test]
async fn stderr_is_logged_before_cancellation_without_waiting_for_exit() {
    let capture = Capture::default();
    agora_core::logger::init(capture.clone(), agora_core::logger::LevelFilter::Error).unwrap();
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
                .run(AgentTask::new("hello"), None, control, &mut Output)
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
}
