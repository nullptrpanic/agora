use super::*;

fn probe() -> &'static Path {
    static PROBE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PROBE
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("inheritance");
            let output = Command::new("/usr/bin/xcrun")
                .args(["clang", "-Wall", "-Wextra", "-Werror", "-pthread", "-o"])
                .arg(&executable)
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/inheritance.c"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (directory, executable)
        })
        .1
}

async fn check(mode: &str) {
    let native = tempfile::tempdir().unwrap();
    assert!(
        Command::new(probe())
            .arg(mode)
            .arg(native.path().join("file"))
            .status()
            .unwrap()
            .success(),
        "native {mode}"
    );
    for plain in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut config = sandbox_config_in(directory.path().join("workspace"));
        config.network.tls = TlsMode::Off;
        if plain {
            config = config.with_plain_workspace();
        }
        let outcome = tokio::time::timeout(
            sandbox_lifecycle_timeout(15),
            Sandbox::new(config, NoopCallback).run(
                SandboxCommand::new(probe())
                    .arg(mode)
                    .arg(directory.path().join("file")),
            ),
        )
        .await
        .expect("descriptor inheritance stalled")
        .unwrap();
        assert!(
            outcome.status().success(),
            "{mode} plain={plain}: {:?}",
            outcome.status()
        );
    }
}

#[tokio::test]
async fn consecutive_exec_preserves_file_stdin_and_stdout() {
    check("exec").await;
}

#[tokio::test]
async fn setexec_launcher_preserves_file_stdin_and_stdout() {
    check("exec-setexec").await;
}

#[tokio::test]
async fn spawn_actions_do_not_capture_descriptors_closed_before_spawn() {
    check("stale").await;
}

#[tokio::test]
async fn spawn_dup2_retains_redirected_output_after_parent_close() {
    check("redirect").await;
}

#[tokio::test]
async fn spawn_cloexec_default_retains_redirected_output_after_parent_close() {
    check("redirect-cloexec").await;
}

#[tokio::test]
async fn spawn_dup2_replaces_the_target_open_description() {
    check("remap").await;
}

#[tokio::test]
async fn spawn_applies_chained_dup2_and_close_actions_in_order() {
    check("remap-chain").await;
}

#[tokio::test]
async fn spawn_open_replaces_only_its_target_alias() {
    check("remap-open").await;
}

#[tokio::test]
async fn spawn_high_descriptor_action_does_not_exhaust_auxiliary_fd_space() {
    check("remap-high-close").await;
}

#[tokio::test]
async fn spawn_replay_preserves_chdir_and_fchdir_actions() {
    for mode in [
        "remap-chdir",
        "remap-chdir-np",
        "remap-fchdir",
        "remap-fchdir-np",
    ] {
        check(mode).await;
    }
}

#[tokio::test]
async fn failed_spawn_releases_temporary_descriptors_and_preserves_parent_files() {
    check("spawn-failure").await;
}

#[tokio::test]
async fn fork_retains_files_opened_by_application_prepare_handlers() {
    check("fork").await;
}
