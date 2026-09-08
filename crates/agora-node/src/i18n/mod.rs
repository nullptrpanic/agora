mod zh_cn;

pub(crate) use zh_cn::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Stopped,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FailureCopy {
    pub category: &'static str,
    pub summary: &'static str,
}

#[cfg(test)]
mod tests;

pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}
