use crate::channel::ChannelReply;
use crate::store::StoreChannelSessionKey;
use anyhow::{Result, anyhow, ensure};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

type Reply = Option<ChannelReply>;
type Outcome = std::result::Result<Reply, String>;
pub(super) type Key = (StoreChannelSessionKey, String);

struct Entry {
    outcome: watch::Receiver<Option<(Instant, Outcome)>>,
    delivered: bool,
    worker: tokio::task::JoinHandle<()>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Default)]
pub(super) struct CommandReplies {
    entries: Mutex<HashMap<Key, Entry>>,
}

impl CommandReplies {
    pub(super) fn delivered(&self, key: &Key) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(key)
        {
            entry.delivered = true;
        }
    }

    // A command can release its ordering barrier before either execution or reply
    // delivery finishes. Keep its operation alive even if an ACK deadline expires.
    pub(super) async fn run(
        &self,
        key: Key,
        admission: &crate::daemon::RouteAdmission,
        execute: impl Future<Output = Result<Reply>> + Send + 'static,
    ) -> Result<Reply> {
        const CAPACITY: usize = 4096;
        const TTL: Duration = Duration::from_secs(600);
        let mut receiver = {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.retain(|_, entry| match &*entry.outcome.borrow() {
                Some((at, outcome)) => at.elapsed() < TTL && outcome.is_ok(),
                None => entry.outcome.has_changed().is_ok(),
            });
            if let Some(existing) = entries.get(&key) {
                admission.complete();
                existing.outcome.clone()
            } else {
                if entries.len() >= CAPACITY {
                    let oldest = entries
                        .iter()
                        .filter(|(_, e)| e.delivered)
                        .filter_map(|(key, e)| Some((e.outcome.borrow().as_ref()?.0, key.clone())))
                        .min_by_key(|(at, _)| *at);
                    if let Some((_, key)) = oldest {
                        entries.remove(&key);
                    }
                }
                ensure!(
                    entries.len() < CAPACITY,
                    "too many unconfirmed command replies"
                );
                let (sender, receiver) = watch::channel(None);
                let worker = tokio::spawn(async move {
                    let outcome = execute.await.map_err(|e| e.to_string());
                    sender.send_replace(Some((Instant::now(), outcome)));
                });
                entries.insert(
                    key,
                    Entry {
                        outcome: receiver.clone(),
                        delivered: false,
                        worker,
                    },
                );
                receiver
            }
        };
        let result = receiver
            .wait_for(Option::is_some)
            .await
            .map_err(|_| anyhow!("command execution interrupted"))?;
        result
            .as_ref()
            .unwrap()
            .1
            .clone()
            .map_err(anyhow::Error::msg)
    }
}
