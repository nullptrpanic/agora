mod card;
mod channel;
mod lark_api;
mod proxy;

pub use channel::{LarkChannel, LarkRun, LarkTask};

#[derive(Clone, Debug, PartialEq, Eq)]
struct LarkReplyTarget {
    message_id: String,
}
