//! Network filesystem protocol, broker, and storage backends.

mod backend;
#[cfg(all(not(agora_sandbox_hook_build), any(feature = "remote-smb", test)))]
pub(crate) mod broker;
pub(crate) mod client;
#[cfg(all(not(agora_sandbox_hook_build), any(feature = "remote-smb", test)))]
pub(crate) mod controller;
pub(crate) mod protocol;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) use crate::ipc as transport;

pub use backend::SmbRemoteConfig;

#[cfg(all(feature = "remote-smb", not(agora_sandbox_hook_build)))]
pub(crate) async fn start_controller(
    remotes: &[SmbRemoteConfig],
    runtime_directory: &std::path::Path,
    preflight_errors: &[Option<libc::c_int>],
) -> anyhow::Result<controller::RemoteController> {
    anyhow::ensure!(
        preflight_errors.len() == remotes.len(),
        "SMB remote probe count does not match configured roots"
    );
    controller::RemoteController::start_with_storage_and_connection_probes(
        backend::configured_storage(remotes),
        runtime_directory,
        preflight_errors,
    )
    .await
}

#[cfg(all(test, feature = "remote-smb", not(agora_sandbox_hook_build)))]
mod tests {
    #[tokio::test]
    async fn configured_controller_accepts_an_empty_remote_list() {
        let runtime = tempfile::tempdir().unwrap();

        let controller = super::start_controller(&[], runtime.path(), &[])
            .await
            .unwrap();

        assert!(controller.runtime().socket().exists());
        controller.shutdown().await.unwrap();
    }
}
