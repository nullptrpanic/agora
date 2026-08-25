//! SMB backend boundary.

mod config;
#[cfg(all(feature = "remote-smb", not(agora_sandbox_hook_build)))]
mod storage;

pub use config::SmbRemoteConfig;
#[cfg(all(feature = "remote-smb", not(agora_sandbox_hook_build)))]
pub(in crate::nfs) use storage::configured_storage;
