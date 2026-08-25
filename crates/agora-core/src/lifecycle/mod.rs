pub mod shutdown;
pub mod signal;

use anyhow::Result;
use shutdown::{ShutdownGuard, ShutdownReason};
use signal::{Signal, SignalHandler, SignalHandlers};
use std::future::Future;
use std::sync::Arc;

impl SignalHandler for Arc<ShutdownGuard> {
    fn handle(&self, signal: Signal) {
        self.shutdown(ShutdownReason::Signal {
            signal: signal.number(),
        });
    }
}

impl ShutdownGuard {
    pub async fn run<F, H>(self: Arc<Self>, process: F, signals: SignalHandlers<H>) -> Result<()>
    where
        F: Future<Output = Result<()>>,
        H: SignalHandler,
    {
        self.run_with_shutdown(process, signals, |_| async {}).await
    }

    pub async fn run_with_shutdown<F, H, C, S>(
        self: Arc<Self>,
        process: F,
        signals: SignalHandlers<H>,
        shutdown: C,
    ) -> Result<()>
    where
        F: Future<Output = Result<()>>,
        H: SignalHandler,
        C: FnOnce(ShutdownReason) -> S,
        S: Future<Output = ()>,
    {
        let lifecycle_error = {
            let signal = signals.run();
            tokio::pin!(process);
            tokio::pin!(signal);

            let (reason, lifecycle_error) = tokio::select! {
                biased;
                reason = shutdown::wait_for_shutdown() => (reason, None),
                result = &mut signal => match result {
                    Ok(signal) => {
                        let reason = ShutdownReason::Signal {
                            signal: signal.number(),
                        };
                        self.shutdown(reason.clone());
                        (reason, None)
                    }
                    Err(err) => {
                        let reason = ShutdownReason::Failed {
                            error: err.to_string(),
                        };
                        self.shutdown(reason.clone());
                        (reason, Some(err.into()))
                    }
                },
                result = &mut process => match result {
                    Ok(()) => {
                        let reason = ShutdownReason::Normal;
                        self.shutdown(reason.clone());
                        (reason, None)
                    }
                    Err(err) => {
                        let reason = ShutdownReason::Failed {
                            error: err.to_string(),
                        };
                        self.shutdown(reason.clone());
                        (reason, Some(err))
                    }
                },
            };
            shutdown(reason).await;
            lifecycle_error
        };

        drop(self);
        match lifecycle_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
