use agora_core::lifecycle::shutdown::{ShutdownGuard, ShutdownReason, on_shutdown};
use agora_core::lifecycle::signal::{Signal, SignalHandler, SignalHandlers};
use std::sync::{Arc, Mutex};

struct NoopSignalHandler;

impl SignalHandler for NoopSignalHandler {
    fn handle(&self, _signal: Signal) {}
}

#[tokio::test]
async fn successful_process_completion_reports_normal_shutdown() {
    let callback_reason = Arc::new(Mutex::new(None));
    let received = Arc::clone(&callback_reason);
    on_shutdown(move |reason| {
        *received.lock().unwrap() = Some(reason);
        Ok(())
    })
    .unwrap();

    ShutdownGuard::get()
        .run(async { Ok(()) }, SignalHandlers::<NoopSignalHandler>::new())
        .await
        .unwrap();

    assert_eq!(
        *callback_reason.lock().unwrap(),
        Some(ShutdownReason::Normal)
    );
}
