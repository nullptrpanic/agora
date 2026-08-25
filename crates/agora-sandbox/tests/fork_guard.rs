#![cfg(target_os = "macos")]

use std::process::Command;

const BOOTSTRAP_CHILD_ENVIRONMENT: &str = "AGORA_SANDBOX_TEST_BOOTSTRAP_CHILD";

#[test]
fn filesystem_hook_survives_process_bootstrap() {
    let hook = env!("AGORA_SANDBOX_EMBEDDED_HOOK_PATH");
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["filesystem_hook_bootstrap_child", "--exact", "--nocapture"])
        .env("DYLD_INSERT_LIBRARIES", hook)
        .env(BOOTSTRAP_CHILD_ENVIRONMENT, "1")
        .status()
        .unwrap();

    assert!(status.success(), "injected child status: {status:?}");
}

#[test]
fn filesystem_hook_bootstrap_child() {
    if let Some(value) = std::env::var_os(BOOTSTRAP_CHILD_ENVIRONMENT) {
        assert_eq!(value, "1");
    }
}
