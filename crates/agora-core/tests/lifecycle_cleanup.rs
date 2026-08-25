use agora_core::lifecycle::shutdown::{ShutdownGuard, ShutdownReason};
use agora_core::lifecycle::signal::{Signal, SignalHandler, SignalHandlers};
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

struct PendingProcess {
    dropped: Arc<AtomicBool>,
}

impl Future for PendingProcess {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Drop for PendingProcess {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct NoopSignalHandler;

impl SignalHandler for NoopSignalHandler {
    fn handle(&self, _signal: Signal) {}
}

#[tokio::test]
async fn async_shutdown_cleanup_runs_before_the_process_future_is_dropped() {
    let process_dropped = Arc::new(AtomicBool::new(false));
    let cleanup_observed_live_process = Arc::new(AtomicBool::new(false));
    let guard = ShutdownGuard::get();
    let shutdown = Arc::clone(&guard);

    tokio::spawn(async move {
        tokio::task::yield_now().await;
        shutdown.shutdown(ShutdownReason::Requested {
            reason: "test".to_string(),
        });
    });

    let observed = Arc::clone(&cleanup_observed_live_process);
    let dropped = Arc::clone(&process_dropped);
    guard
        .run_with_shutdown(
            PendingProcess {
                dropped: Arc::clone(&process_dropped),
            },
            SignalHandlers::<NoopSignalHandler>::new(),
            move |reason| async move {
                assert_eq!(
                    reason,
                    ShutdownReason::Requested {
                        reason: "test".to_string(),
                    }
                );
                observed.store(!dropped.load(Ordering::SeqCst), Ordering::SeqCst);
            },
        )
        .await
        .unwrap();

    assert!(cleanup_observed_live_process.load(Ordering::SeqCst));
    assert!(process_dropped.load(Ordering::SeqCst));
}
