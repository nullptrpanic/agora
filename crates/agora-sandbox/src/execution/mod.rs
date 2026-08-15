mod controller;
mod protocol;
mod store;

pub(crate) const DEFAULT_EXECUTABLE_PATH: &str = "/usr/bin:/bin";

pub(crate) use controller::ExecutionController;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(crate) use protocol::encode_ping_request;
#[cfg(test)]
pub(crate) use protocol::{EXECUTION_PROTOCOL_VERSION, decode_prepare_request};
pub(crate) use protocol::{
    PrepareResponse, decode_prepare_response, encode_prepare_request, frame_length,
};
pub(crate) use store::{resolve_executable, resolve_shebang};

#[cfg(test)]
mod tests;
