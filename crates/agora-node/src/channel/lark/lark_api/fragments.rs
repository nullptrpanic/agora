use super::LarkFrame;
use anyhow::{Result, anyhow, ensure};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_MESSAGES: usize = 64;
const MAX_PARTS: usize = 64;
const MAX_BYTES: usize = 1024 * 1024;
const TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct LarkFragments {
    messages: HashMap<String, PendingMessage>,
}

struct PendingMessage {
    started: Instant,
    parts: Vec<Option<Vec<u8>>>,
}

impl LarkFragments {
    // Owned by one connection. Incomplete data must never reach JSON parsing or ACK 200.
    pub(super) fn reassemble(&mut self, frame: &mut LarkFrame) -> Result<bool> {
        self.messages
            .retain(|_, message| message.started.elapsed() < TTL);
        let number = |name, default| -> Result<usize> {
            frame.header(name).map_or(Ok(default), |value| {
                value
                    .parse()
                    .map_err(|_| anyhow!("invalid fragment {name}"))
            })
        };
        let count = number("sum", 1)?;
        let sequence = number("seq", 0)?;
        ensure!(
            (1..=MAX_PARTS).contains(&count) && sequence < count,
            "invalid fragment range"
        );
        ensure!(
            frame.payload.len() <= MAX_BYTES,
            "fragment exceeds byte limit"
        );
        if count == 1 {
            return Ok(true);
        }
        ensure!(frame.header("seq").is_some(), "fragment missing seq");
        let id = frame
            .header("message_id")
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(|| anyhow!("fragment missing or oversized message_id"))?
            .to_string();
        if let Some(message) = self.messages.get(&id) {
            let conflict = message.parts.len() != count
                || message
                    .parts
                    .get(sequence)
                    .and_then(Option::as_ref)
                    .is_some_and(|previous| previous != &frame.payload);
            if conflict {
                self.messages.remove(&id);
                return Err(anyhow!("conflicting fragments"));
            }
            if message.parts[sequence].is_some() {
                return Ok(false);
            }
        } else {
            ensure!(
                self.messages.len() < MAX_MESSAGES,
                "fragment message limit reached"
            );
        }
        let used = self
            .messages
            .values()
            .flat_map(|message| &message.parts)
            .flatten()
            .map(Vec::len)
            .sum::<usize>();
        if used.saturating_add(frame.payload.len()) > MAX_BYTES {
            self.messages.remove(&id);
            return Err(anyhow!("fragment cache byte limit reached"));
        }
        let message = self
            .messages
            .entry(id.clone())
            .or_insert_with(|| PendingMessage {
                started: Instant::now(),
                parts: vec![None; count],
            });
        message.parts[sequence] = Some(std::mem::take(&mut frame.payload));
        if message.parts.iter().any(Option::is_none) {
            return Ok(false);
        }
        let complete = self
            .messages
            .remove(&id)
            .expect("completed fragment is cached");
        frame.payload = complete.parts.into_iter().flatten().flatten().collect();
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
