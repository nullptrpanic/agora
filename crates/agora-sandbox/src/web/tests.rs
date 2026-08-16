use super::{WebOptions, run_with};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn executable(path: &Path) {
    fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).unwrap();
}

fn test_options(root: &Path, open_browser: bool) -> (WebOptions, std::path::PathBuf) {
    let config = root.join("sandbox.json");
    let sandbox = root.join("agora-sandbox");
    fs::write(
        &config,
        format!(
            r#"{{"workdir":"{}","log":{{"file":"runtime/sandbox.log"}}}}"#,
            root.display()
        ),
    )
    .unwrap();
    executable(&sandbox);
    (
        WebOptions {
            config_path: config,
            log_path: root.join("runtime/sandbox.log"),
            open_browser,
        },
        sandbox,
    )
}

#[tokio::test]
async fn orchestration_opens_a_fragment_token_on_a_random_loopback_listener() {
    let root = tempfile::tempdir().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let opener_capture = captured.clone();
    let (options, sandbox) = test_options(root.path(), true);

    run_with(
        options,
        sandbox,
        move |url| {
            *opener_capture.lock().unwrap() = Some(url);
            async { Ok(()) }
        },
        async {},
    )
    .await
    .unwrap();

    let url = captured.lock().unwrap().clone().unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    assert!(url.contains("/#token="));
    assert!(!url.contains(&root.path().display().to_string()));
}

#[tokio::test]
async fn no_open_skips_browser_and_browser_failure_does_not_stop_the_server() {
    let root = tempfile::tempdir().unwrap();
    let called = Arc::new(Mutex::new(0_u8));
    let no_open_called = called.clone();
    let (options, sandbox) = test_options(root.path(), false);
    run_with(
        options,
        sandbox,
        move |_url| {
            *no_open_called.lock().unwrap() += 1;
            async { Ok(()) }
        },
        async {},
    )
    .await
    .unwrap();
    assert_eq!(*called.lock().unwrap(), 0);

    let (options, sandbox) = test_options(root.path(), true);
    run_with(
        options,
        sandbox,
        |_url| async { Err(io::Error::other("browser unavailable")) },
        async {},
    )
    .await
    .unwrap();
}
