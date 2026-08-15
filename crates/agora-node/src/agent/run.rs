use super::AgentOutcome;
use std::fmt;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRunCancellation {
    Stopped,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentRunState {
    Running,
    Cancelled(AgentRunCancellation),
}

#[derive(Clone)]
pub struct AgentRunControl {
    state: watch::Sender<AgentRunState>,
}

impl Default for AgentRunControl {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRunControl {
    pub fn new() -> Self {
        let (state, _) = watch::channel(AgentRunState::Running);
        Self { state }
    }

    pub fn stop(&self) -> bool {
        self.cancel(AgentRunCancellation::Stopped)
    }

    pub fn interrupt(&self) -> bool {
        self.cancel(AgentRunCancellation::Interrupted)
    }

    pub async fn cancelled(&self) -> AgentRunCancellation {
        let mut state = self.state.subscribe();
        loop {
            if let AgentRunState::Cancelled(cancellation) = *state.borrow() {
                return cancellation;
            }
            if state.changed().await.is_err() {
                return AgentRunCancellation::Interrupted;
            }
        }
    }

    fn cancel(&self, cancellation: AgentRunCancellation) -> bool {
        self.state.send_if_modified(|state| {
            if *state != AgentRunState::Running {
                return false;
            }
            *state = AgentRunState::Cancelled(cancellation);
            true
        })
    }
}

impl fmt::Debug for AgentRunControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRunControl")
            .field("state", &*self.state.borrow())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentRunOutcome {
    Completed(AgentOutcome),
    Cancelled(AgentRunCancellation),
}
