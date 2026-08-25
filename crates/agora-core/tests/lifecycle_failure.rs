use agora_core::lifecycle::shutdown::{ShutdownGuard, ShutdownReason, on_shutdown};
use agora_core::lifecycle::signal::{Signal, SignalHandler, SignalHandlers};
use anyhow::anyhow;
use std::sync::{Arc, Mutex};

struct NoopSignalHandler;

impl SignalHandler for NoopSignalHandler {
    fn handle(&self, _signal: Signal) {}
}

#[tokio::test]
async fn process_failure_is_returned_after_shutdown_cleanup() {
    let received = Arc::new(Mutex::new(None));
    let callback_received = Arc::clone(&received);
    on_shutdown(move |reason| {
        *callback_received.lock().unwrap() = Some(reason);
        Ok(())
    })
    .unwrap();
    let guard = ShutdownGuard::get();

    let error = guard
        .run_with_shutdown(
            async { Err(anyhow!("process failed")) },
            SignalHandlers::<NoopSignalHandler>::new(),
            |reason| async move {
                assert_eq!(
                    reason,
                    ShutdownReason::Failed {
                        error: "process failed".to_string(),
                    }
                );
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "process failed");
    assert_eq!(
        *received.lock().unwrap(),
        Some(ShutdownReason::Failed {
            error: "process failed".to_string(),
        })
    );
}
