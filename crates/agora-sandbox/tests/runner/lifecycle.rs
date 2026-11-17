use super::*;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Stdio;

fn probe() -> &'static Path {
    static PROBE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PROBE
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("lifecycle");
            let output = Command::new("/usr/bin/xcrun")
                .args(["clang", "-Wall", "-Wextra", "-Werror", "-pthread", "-o"])
                .arg(&executable)
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lifecycle.c"))
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

async fn run_probe(mode: &str, plain: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("object");
    if mode == "cwd_lower" {
        std::fs::create_dir(&path).unwrap();
    }
    let mut config = sandbox_config_in(directory.path().join("workspace"));
    config.network.tls = TlsMode::Off;
    if plain {
        config = config.with_plain_workspace();
    }
    let mut command = SandboxCommand::new(probe()).arg(mode).arg(&path);
    if mode == "fifo" {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // A native inherited writer lets the forked child release the FIFO.
        // It must not have to fork before an existing reader can finish.
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        command = command.stdin(Stdio::from(writer));
    }
    let outcome = tokio::time::timeout(
        sandbox_lifecycle_timeout(10),
        Sandbox::new(config, NoopCallback).run(command),
    )
    .await
    .expect("lifecycle probe stalled")
    .unwrap();
    assert!(
        outcome.status().success(),
        "{mode} (plain={plain}): {:?}",
        outcome.status()
    );
}

#[tokio::test]
async fn normal_exit_preserves_both_stdio_flushes() {
    run_probe("stdio", false).await;
}

#[tokio::test]
async fn tracked_fifo_read_does_not_prevent_another_thread_from_forking() {
    for plain in [true, false] {
        run_probe("fifo", plain).await;
    }
}

#[tokio::test]
async fn partial_page_munmap_keeps_remaining_mapping_syncable() {
    run_probe("munmap", false).await;
}

#[tokio::test]
async fn readonly_shared_mapping_cannot_gain_write_permission() {
    run_probe("readonly", false).await;
}

#[tokio::test]
async fn fchmod_on_unlinked_descriptor_does_not_change_replacement() {
    for plain in [true, false] {
        run_probe("fchmod", plain).await;
    }
}

#[tokio::test]
async fn relative_open_follows_renamed_current_directory() {
    for plain in [true, false] {
        run_probe("cwd", plain).await;
        run_probe("cwd_lower", plain).await;
    }
}

#[tokio::test]
async fn encrypted_relative_record_locks_use_the_logical_offset() {
    run_probe("locks", false).await;
}
