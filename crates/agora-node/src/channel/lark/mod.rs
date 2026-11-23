mod card;
mod channel;
mod context;
mod lark_api;
mod proxy;
mod reply;

pub use channel::{LarkChannel, LarkRun, LarkSender, LarkTask};

#[derive(Clone, Debug, PartialEq, Eq)]
struct LarkReplyTarget {
    message_id: String,
    mention_user_id: Option<String>,
}

impl LarkReplyTarget {
    fn mention_id(&self) -> anyhow::Result<Option<&str>> {
        let Some(id) = self.mention_user_id.as_deref() else {
            return Ok(None);
        };
        anyhow::ensure!(
            !id.trim().is_empty() && id.trim() != "all" && id.len() <= 512,
            "lark reply requires a single sender identity"
        );
        Ok(Some(id))
    }

    fn escaped_mention_id(&self) -> anyhow::Result<Option<String>> {
        Ok(self.mention_id()?.map(|id| {
            id.replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        }))
    }
}
