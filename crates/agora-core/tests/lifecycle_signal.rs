#![cfg(unix)]

use agora_core::lifecycle::shutdown::{ShutdownGuard, ShutdownReason, on_shutdown};
use agora_core::lifecycle::signal::{Signal, SignalHandlers};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test]
async fn sigterm_reason_is_delivered_to_shutdown_callbacks() {
    let received = Arc::new(Mutex::new(None));
    let callback_received = Arc::clone(&received);
    on_shutdown(move |reason| {
        *callback_received.lock().unwrap() = Some(reason);
        Ok(())
    })
    .unwrap();

    let guard = ShutdownGuard::get();
    let signal_number = tokio::signal::unix::SignalKind::terminate().as_raw_value();
    let mut signals = SignalHandlers::new();
    signals
        .register(Signal::new(signal_number), Arc::clone(&guard))
        .unwrap();

    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(std::process::id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    });

    guard
        .run(std::future::pending::<anyhow::Result<()>>(), signals)
        .await
        .unwrap();

    assert_eq!(
        *received.lock().unwrap(),
        Some(ShutdownReason::Signal {
            signal: signal_number,
        })
    );
}
