use uuid::Uuid;

pub(crate) const TRACE_ID_ENVIRONMENT: &str = "AGORA_SANDBOX_TRACE_ID";
pub(crate) const TRACE_ID_HEADER: &str = "Agora-Trace-Id";
const MAX_TRACE_ENTRIES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TraceContext {
    ids: Vec<String>,
}

impl TraceContext {
    pub(crate) fn root() -> Self {
        Self {
            ids: vec![Uuid::new_v4().to_string()],
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let ids = value
            .split(',')
            .map(str::trim)
            .map(str::to_string)
            .collect::<Vec<_>>();
        Self::new(ids)
    }

    pub(crate) fn new(ids: Vec<String>) -> Result<Self, String> {
        if ids.is_empty() || ids.len() > MAX_TRACE_ENTRIES {
            return Err(format!(
                "trace id chain must contain between 1 and {MAX_TRACE_ENTRIES} entries"
            ));
        }
        if ids.iter().any(|id| !Self::valid_id(id)) {
            return Err("trace id chain contains an invalid entry".to_string());
        }
        Ok(Self { ids })
    }

    pub(crate) fn child(&self) -> Self {
        let mut ids = self.ids.clone();
        if ids.len() == MAX_TRACE_ENTRIES {
            ids.remove(0);
        }
        ids.push(Uuid::new_v4().to_string());
        Self { ids }
    }

    pub(crate) fn encode(&self) -> String {
        self.ids.join(", ")
    }

    fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 128
            && id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
            })
    }
}

#[cfg(test)]
mod tests;
