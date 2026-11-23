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
