use crate::logger;
use anyhow::{Result, anyhow};
use std::fmt::{self, Display, Formatter};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use tokio::sync::Notify;

type ShutdownCallback = Box<dyn FnOnce(ShutdownReason) -> Result<()> + Send>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
    Signal { signal: i32 },
    Requested { reason: String },
    Normal,
    Failed { error: String },
}

impl Display for ShutdownReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signal { signal } => write!(formatter, "signal:{signal}"),
            Self::Requested { reason } => write!(formatter, "requested:{reason}"),
            Self::Normal => formatter.write_str("normal"),
            Self::Failed { error } => write!(formatter, "failed:{error}"),
        }
    }
}

#[derive(Default)]
struct ShutdownState {
    callbacks: Vec<ShutdownCallback>,
    reason: Option<ShutdownReason>,
    finished: bool,
}

#[derive(Default)]
struct ShutdownRegistry {
    state: Mutex<ShutdownState>,
    requested: Notify,
}

impl ShutdownRegistry {
    fn state(&self) -> MutexGuard<'_, ShutdownState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register(&self, callback: ShutdownCallback) -> Result<()> {
        let mut state = self.state();
        if state.finished || state.reason.is_some() {
            return Err(anyhow!("process shutdown has already started"));
        }
        state.callbacks.push(callback);
        Ok(())
    }

    fn shutdown(&self, reason: ShutdownReason) -> bool {
        let mut state = self.state();
        if state.finished || state.reason.is_some() {
            return false;
        }
        state.reason = Some(reason);
        drop(state);
        self.requested.notify_one();
        true
    }

    async fn wait(&self) -> ShutdownReason {
        loop {
            let notified = self.requested.notified();
            if let Some(reason) = self.state().reason.clone() {
                return reason;
            }
            notified.await;
        }
    }

    fn finish(&self) -> Option<(ShutdownReason, Vec<ShutdownCallback>)> {
        let mut state = self.state();
        if state.finished {
            return None;
        }
        state.finished = true;
        let reason = state.reason.clone().unwrap_or(ShutdownReason::Normal);
        Some((reason, std::mem::take(&mut state.callbacks)))
    }
}

fn registry() -> &'static ShutdownRegistry {
    static REGISTRY: OnceLock<ShutdownRegistry> = OnceLock::new();
    REGISTRY.get_or_init(ShutdownRegistry::default)
}

pub struct ShutdownGuard;

impl ShutdownGuard {
    pub fn get() -> Arc<Self> {
        static INSTANCE: OnceLock<Mutex<Weak<ShutdownGuard>>> = OnceLock::new();

        let mut instance = INSTANCE
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(guard) = instance.upgrade() {
            return guard;
        }

        let guard = Arc::new(Self);
        *instance = Arc::downgrade(&guard);
        guard
    }

    pub fn shutdown(&self, reason: ShutdownReason) -> bool {
        registry().shutdown(reason)
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        let Some((reason, callbacks)) = registry().finish() else {
            return;
        };

        logger::info!("process shutdown started reason={}", reason);
        for callback in callbacks {
            match catch_unwind(AssertUnwindSafe(|| callback(reason.clone()))) {
                Ok(Ok(())) => {}
                Ok(Err(err)) => logger::error!("shutdown callback failed: {}", err),
                Err(_) => logger::error!("shutdown callback panicked"),
            }
        }
    }
}

pub fn on_shutdown<F>(callback: F) -> Result<()>
where
    F: FnOnce(ShutdownReason) -> Result<()> + Send + 'static,
{
    registry().register(Box::new(callback))
}

pub fn request_shutdown(reason: impl Into<String>) -> bool {
    ShutdownGuard::get().shutdown(ShutdownReason::Requested {
        reason: reason.into(),
    })
}

pub(super) async fn wait_for_shutdown() -> ShutdownReason {
    registry().wait().await
}
